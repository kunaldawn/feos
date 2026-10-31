//! Application-folder scanning.

use crate::{Bundle, BundleError, Icon, Manifest};

/// One entry in an application directory.
///
/// The desktop supplies these from whatever storage the board provides. The
/// current QEMU board has no filesystem, so its application folder is a list
/// of bundles embedded in the kernel image; the scanner is identical for a
/// future disk or RAM filesystem.
#[derive(Clone, Copy)]
pub struct DirEntry<'a> {
    /// File name, used for diagnostics and extension filtering.
    pub name: &'a str,
    /// Raw file contents.
    pub data: &'a [u8],
}

impl<'a> DirEntry<'a> {
    /// Creates a directory entry.
    pub const fn new(name: &'a str, data: &'a [u8]) -> Self {
        Self { name, data }
    }
}

/// Summary returned by [`AppCatalog::scan`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ScanReport {
    /// Files that looked like bundles and produced an installed app.
    pub installed: usize,
    /// Files skipped because they were invalid, duplicate or unsupported.
    pub skipped: usize,
}

/// One application discovered in the application folder.
#[derive(Clone, Copy)]
pub struct InstalledApp {
    /// Parsed manifest.
    pub manifest: Manifest,
    /// Decoded icon, or [`Icon::EMPTY`] when the bundle had none.
    pub icon: Icon,
    /// Whether a usable icon was decoded.
    pub has_icon: bool,
}

impl InstalledApp {
    /// An empty catalog slot.
    pub const EMPTY: Self = Self {
        manifest: Manifest::EMPTY,
        icon: Icon::EMPTY,
        has_icon: false,
    };

    /// The application identifier.
    pub fn id(&self) -> &str {
        self.manifest.id.as_str()
    }

    /// The display name.
    pub fn name(&self) -> &str {
        self.manifest.name.as_str()
    }

    /// The shell entry point.
    pub fn entry(&self) -> &str {
        self.manifest.entry.as_str()
    }
}

/// A fixed-capacity catalog of installed applications.
///
/// `N` is the maximum number of bundles that can be installed at once. The
/// catalog is populated by [`scan`](AppCatalog::scan) and owns copies of the
/// manifests and icons, so the original bundle bytes can be discarded.
pub struct AppCatalog<const N: usize = 12> {
    apps: [InstalledApp; N],
    len: usize,
    skipped: usize,
}

impl<const N: usize> AppCatalog<N> {
    /// Creates an empty catalog.
    pub const fn new() -> Self {
        Self {
            apps: [InstalledApp::EMPTY; N],
            len: 0,
            skipped: 0,
        }
    }

    /// Clears the catalog and scans `entries`.
    ///
    /// Every file is tried, not just names ending in `.fapp`: the bundle
    /// format is detected from the archive magic, so a renamed bundle still
    /// appears. Files that are not bundles are ignored silently; archives
    /// that look like bundles but are invalid are counted as skipped.
    pub fn scan(&mut self, entries: &[DirEntry<'_>]) -> ScanReport {
        self.len = 0;
        self.skipped = 0;
        let mut installed = 0;

        for entry in entries {
            if entry.name.is_empty() || entry.name.starts_with('.') {
                continue;
            }

            let bundle = match Bundle::open(entry.data) {
                Ok(bundle) => bundle,
                Err(BundleError::NotABundle) => continue,
                Err(_) => {
                    self.skipped += 1;
                    continue;
                }
            };

            let manifest = match bundle.manifest() {
                Ok(manifest) => manifest,
                Err(_) => {
                    self.skipped += 1;
                    continue;
                }
            };

            if manifest.id.is_empty() || self.find_id(manifest.id.as_str()).is_some() {
                self.skipped += 1;
                continue;
            }

            if self.len == N {
                self.skipped += 1;
                continue;
            }

            let icon = bundle.icon(&manifest).unwrap_or(Icon::EMPTY);
            self.apps[self.len] = InstalledApp {
                manifest,
                icon,
                has_icon: !icon.is_empty(),
            };
            self.len += 1;
            installed += 1;
        }

        ScanReport {
            installed,
            skipped: self.skipped,
        }
    }

    /// Number of installed applications.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// Returns `true` when no applications were found.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Number of files skipped during the last scan.
    pub const fn skipped(&self) -> usize {
        self.skipped
    }

    /// Returns the application at `index`.
    pub fn get(&self, index: usize) -> Option<&InstalledApp> {
        self.apps.get(index).filter(|_| index < self.len)
    }

    /// Returns a mutable application at `index`.
    pub fn get_mut(&mut self, index: usize) -> Option<&mut InstalledApp> {
        if index < self.len {
            self.apps.get_mut(index)
        } else {
            None
        }
    }

    /// Finds an application by identifier.
    pub fn find_id(&self, id: &str) -> Option<usize> {
        self.apps[..self.len]
            .iter()
            .position(|app| app.manifest.id.eq_str(id))
    }

    /// Iterates over installed applications.
    pub fn iter(&self) -> impl Iterator<Item = &InstalledApp> {
        self.apps[..self.len].iter()
    }

    /// Returns the first `count` applications, for quick-launch and desktop
    /// shortcuts.
    pub fn take(&self, count: usize) -> &[InstalledApp] {
        &self.apps[..self.len.min(count)]
    }
}

impl<const N: usize> Default for AppCatalog<N> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use feos_archive::ZipWriter;

    fn bundle_bytes(manifest: &str) -> std::vec::Vec<u8> {
        let mut out = [0u8; 8192];
        let mut writer = ZipWriter::new(&mut out);
        writer
            .write_stored("manifest.yaml", manifest.as_bytes())
            .unwrap();
        let len = writer.finish().unwrap();
        out[..len].to_vec()
    }

    #[test]
    fn scans_bundles_and_skips_duplicates() {
        let bytes = bundle_bytes("id: demo\nname: Demo\nentry: gui\n");
        let mut catalog = AppCatalog::<4>::new();
        let report = catalog.scan(&[
            DirEntry::new("demo.fapp", &bytes),
            DirEntry::new("renamed.bin", &bytes),
            DirEntry::new("notes.txt", b"hello"),
            DirEntry::new(".hidden", &bytes),
        ]);

        assert_eq!(report.installed, 1);
        assert_eq!(report.skipped, 1);
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog.get(0).unwrap().name(), "Demo");
        assert_eq!(catalog.find_id("demo"), Some(0));
        assert!(catalog.get(1).is_none());
    }

    #[test]
    fn scans_empty_directory() {
        let mut catalog = AppCatalog::<4>::new();
        let report = catalog.scan(&[]);
        assert_eq!(report.installed, 0);
        assert!(catalog.is_empty());
    }
}
