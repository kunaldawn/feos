//! Single-file application bundles for the feos desktop.
//!
//! A `.fapp` bundle is one compressed archive (ZIP or TAR) that carries
//! everything an application needs:
//!
//! ```text
//! manifest.yaml   id, name, version, entry point, category, description
//! icon.bmp        application icon (BMP, PNG, JPEG, PPM or QOI)
//! ...             any additional resources
//! ```
//!
//! The desktop scans an application directory, detects bundles by their magic
//! bytes rather than by their file name, reads the manifest and decodes the
//! icon. Nothing is allocated: the manifest and icon are copied into
//! fixed-size structures owned by the caller.
//!
//! ```
//! use feos_bundle::{AppCatalog, DirEntry};
//!
//! let mut catalog = AppCatalog::<8>::new();
//! let report = catalog.scan(&[DirEntry::new("demo.fapp", &[])]);
//! assert_eq!(report.installed, 0);
//! ```
//!
//! # Manifest
//!
//! ```yaml
//! id: terminal
//! name: Terminal
//! version: 1.0.0
//! entry: gui
//! category: System
//! description: Run the feos control center
//! icon: icon.bmp
//! args: ""
//! window:
//!   width: 640
//!   height: 420
//! ```
//!
//! `entry` names an `App` in the shell registry. The desktop uses it when
//! launching the application; the bundle format itself is independent of the
//! registry so a future native loader can replace it.

#![cfg_attr(not(test), no_std)]
#![forbid(unsafe_code)]

mod catalog;
mod icon;
pub mod install;
mod manifest;

use core::str;

pub use catalog::{AppCatalog, DirEntry, InstalledApp, ScanReport};
pub use feos_text::Text;
pub use icon::{Icon, IconImage, MAX_ICON};
pub use manifest::{Manifest, MANIFEST_MAX};

use feos_archive::{ArchiveError, TarReader, ZipReader};
use feos_image::ImageError;
use feos_yaml::YamlError;

/// Largest manifest accepted from a bundle, in bytes.
pub const MANIFEST_LIMIT: usize = 4096;

/// Largest encoded icon file accepted from a bundle, in bytes.
pub const ICON_FILE_LIMIT: usize = 16 * 1024;

/// Working memory reserved for image decoding.
const ICON_SCRATCH: usize = 16 * 1024;

/// The archive container used by a bundle.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BundleFormat {
    /// A ZIP archive (the recommended format).
    Zip,
    /// An uncompressed TAR archive.
    Tar,
}

/// Errors produced while opening or reading a bundle.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BundleError {
    /// The file is not a recognised archive.
    NotABundle,
    /// The archive itself is malformed.
    Archive(ArchiveError),
    /// The manifest is not valid YAML.
    Yaml(YamlError),
    /// The icon is not a supported image.
    Image(ImageError),
    /// The bundle has no `manifest.yaml`.
    MissingManifest,
    /// A file named by the manifest is absent.
    MissingFile,
    /// A required manifest field is absent.
    MissingField(&'static str),
    /// A manifest field does not fit its fixed-capacity slot.
    FieldTooLong(&'static str),
    /// A manifest field is a sequence or mapping, not text.
    InvalidField(&'static str),
    /// The manifest is not valid UTF-8.
    InvalidUtf8,
    /// The encoded manifest is larger than [`MANIFEST_LIMIT`].
    ManifestTooLarge,
    /// The icon is larger than [`MAX_ICON`] in either dimension.
    IconTooLarge,
    /// A caller-provided buffer is too small.
    BufferTooSmall {
        /// Bytes required.
        required: usize,
        /// Bytes available.
        available: usize,
    },
}

impl From<ArchiveError> for BundleError {
    fn from(error: ArchiveError) -> Self {
        Self::Archive(error)
    }
}

impl From<YamlError> for BundleError {
    fn from(error: YamlError) -> Self {
        Self::Yaml(error)
    }
}

impl From<ImageError> for BundleError {
    fn from(error: ImageError) -> Self {
        Self::Image(error)
    }
}

/// Result type used by this crate.
pub type Result<T> = core::result::Result<T, BundleError>;

/// A validated bundle backed by its original bytes.
#[derive(Clone, Copy)]
pub struct Bundle<'a> {
    data: &'a [u8],
    format: BundleFormat,
}

impl<'a> Bundle<'a> {
    /// Detects the container format from the leading bytes.
    pub fn detect(data: &'a [u8]) -> Option<BundleFormat> {
        if data.starts_with(&[0x50, 0x4b, 0x03, 0x04])
            || data.starts_with(&[0x50, 0x4b, 0x05, 0x06])
        {
            Some(BundleFormat::Zip)
        } else if data.len() >= 512 && &data[257..262] == b"ustar" {
            Some(BundleFormat::Tar)
        } else {
            None
        }
    }

    /// Opens a bundle after detecting its format.
    pub fn open(data: &'a [u8]) -> Result<Self> {
        let format = Self::detect(data).ok_or(BundleError::NotABundle)?;
        Ok(Self { data, format })
    }

    /// The detected container format.
    pub const fn format(&self) -> BundleFormat {
        self.format
    }

    /// Reads one file from the bundle into `out`.
    ///
    /// Paths are compared after removing a leading `./` or `/`, so both
    /// `manifest.yaml` and `./manifest.yaml` resolve to the same member.
    pub fn read_file(&self, name: &str, out: &mut [u8]) -> Result<usize> {
        match self.format {
            BundleFormat::Zip => self.read_zip(name, out),
            BundleFormat::Tar => self.read_tar(name, out),
        }
    }

    fn read_zip(&self, name: &str, out: &mut [u8]) -> Result<usize> {
        let mut reader = ZipReader::open(self.data)?;
        while let Some(entry) = reader.next()? {
            if entry.is_dir() {
                continue;
            }
            let Ok(path) = entry.name() else {
                continue;
            };
            if path_matches(path, name) {
                return Ok(entry.extract(out)?);
            }
        }
        Err(BundleError::MissingFile)
    }

    fn read_tar(&self, name: &str, out: &mut [u8]) -> Result<usize> {
        let mut reader = TarReader::new(self.data);
        let mut path_buffer = [0u8; 256];
        while let Some(entry) = reader.next()? {
            if !entry.is_file() {
                continue;
            }
            let Ok(path) = entry.path(&mut path_buffer) else {
                continue;
            };
            if path_matches(path, name) {
                if out.len() < entry.data.len() {
                    return Err(BundleError::BufferTooSmall {
                        required: entry.data.len(),
                        available: out.len(),
                    });
                }
                out[..entry.data.len()].copy_from_slice(entry.data);
                return Ok(entry.data.len());
            }
        }
        Err(BundleError::MissingFile)
    }

    /// Reads and parses `manifest.yaml`.
    pub fn manifest(&self) -> Result<Manifest> {
        let mut buffer = [0u8; MANIFEST_LIMIT];
        let len = match self.read_file("manifest.yaml", &mut buffer) {
            Ok(len) => len,
            Err(BundleError::MissingFile) => return Err(BundleError::MissingManifest),
            Err(
                BundleError::BufferTooSmall { .. }
                | BundleError::Archive(ArchiveError::BufferTooSmall { .. }),
            ) => return Err(BundleError::ManifestTooLarge),
            Err(error) => return Err(error),
        };
        let text = str::from_utf8(&buffer[..len]).map_err(|_| BundleError::InvalidUtf8)?;
        Manifest::parse(text)
    }

    /// Reads and decodes the icon named by `manifest`.
    ///
    /// An empty icon path is not an error; it returns [`Icon::EMPTY`] so a
    /// bundle without artwork still appears in the catalog.
    pub fn icon(&self, manifest: &Manifest) -> Result<Icon> {
        if manifest.icon.is_empty() {
            return Ok(Icon::EMPTY);
        }
        let mut encoded = [0u8; ICON_FILE_LIMIT];
        let len = self.read_file(manifest.icon.as_str(), &mut encoded)?;
        let mut icon = Icon::decode(&encoded[..len])?;
        // Hand-drawn smaller versions, optional and beside the main one.
        for extra in [manifest.icon_medium.as_str(), manifest.icon_small.as_str()] {
            if extra.is_empty() {
                continue;
            }
            if let Ok(len) = self.read_file(extra, &mut encoded) {
                icon.add_size(&encoded[..len]);
            }
        }
        Ok(icon)
    }
}

fn path_matches(path: &str, wanted: &str) -> bool {
    let path = path.trim_start_matches("./").trim_start_matches('/');
    let wanted = wanted.trim_start_matches("./").trim_start_matches('/');
    path == wanted
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_containers() {
        assert_eq!(
            Bundle::detect(&[0x50, 0x4b, 0x03, 0x04, 0, 0]),
            Some(BundleFormat::Zip)
        );
        assert_eq!(Bundle::detect(b"not an archive"), None);
    }

    #[test]
    fn oversized_manifest_is_reported_as_such() {
        let manifest = std::vec![b'#'; MANIFEST_LIMIT + 1];
        let mut out = std::vec![0u8; MANIFEST_LIMIT + 1024];
        let mut writer = feos_archive::ZipWriter::new(&mut out);
        writer.write_stored("manifest.yaml", &manifest).unwrap();
        let len = writer.finish().unwrap();
        let bundle = Bundle::open(&out[..len]).unwrap();
        assert_eq!(bundle.manifest().err(), Some(BundleError::ManifestTooLarge));
    }

    #[test]
    fn path_matching_normalises_prefixes() {
        assert!(path_matches("./manifest.yaml", "manifest.yaml"));
        assert!(path_matches("/manifest.yaml", "manifest.yaml"));
        assert!(!path_matches("manifest.yaml", "icon.bmp"));
    }
}
