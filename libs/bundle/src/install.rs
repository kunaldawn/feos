//! Checks that decide whether a `.fapp` may be installed at run time.
//!
//! A package arrives with a SHA-256 digest from somewhere other than the package
//! itself, and only a whole, matching file gets installed. The digest stays beside
//! it, so that a later scan notices if the disk has changed the file since.

use feos_compress::sha256::{hex, sha256};

use crate::{Bundle, BundleError, Manifest};

/// Where installed packages live.
pub const APPS_DIR: &str = "/apps";
/// A package's file name ends in this.
pub const PACKAGE_SUFFIX: &str = ".fapp";
/// Its digest is in a file with this added to the package's name.
pub const DIGEST_SUFFIX: &str = ".sha256";
/// The largest package accepted. They're icons and a manifest: code isn't loaded.
pub const MAX_PACKAGE_BYTES: usize = 8 * 1024 * 1024;

/// Why a package was refused.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum InstallError {
    /// Larger than [`MAX_PACKAGE_BYTES`].
    TooLarge,
    /// The digest supplied isn't one.
    BadDigest,
    /// The file doesn't match its digest.
    DigestMismatch,
    /// The manifest's `id` can't be a file name.
    BadId,
    /// The file isn't a usable bundle.
    Bundle(BundleError),
}

impl core::fmt::Display for InstallError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::TooLarge => f.write_str("package is too large"),
            Self::BadDigest => f.write_str("not a SHA-256 digest"),
            Self::DigestMismatch => f.write_str("package does not match its digest"),
            Self::BadId => f.write_str("manifest id can't be used as a file name"),
            Self::Bundle(error) => write!(f, "not a usable bundle: {error:?}"),
        }
    }
}

/// True for 64 hex digits.
fn is_digest(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// The digest out of a file in `sha256sum` format, or a bare digest.
pub fn parse_digest(text: &str) -> Option<&str> {
    text.split_whitespace().next().filter(|word| is_digest(word))
}

/// True when an `id` is safe to build a path from: `id: ../x` must not choose
/// where a package lands.
pub fn is_safe_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('.')
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// Checks a package against its digest, and returns what it says it is.
pub fn verify_package(data: &[u8], expected: &str) -> Result<Manifest, InstallError> {
    if data.len() > MAX_PACKAGE_BYTES {
        return Err(InstallError::TooLarge);
    }
    if !is_digest(expected) {
        return Err(InstallError::BadDigest);
    }
    if !hex(&sha256(data)).eq_ignore_ascii_case(expected) {
        return Err(InstallError::DigestMismatch);
    }
    let bundle = Bundle::open(data).map_err(InstallError::Bundle)?;
    let manifest = bundle.manifest().map_err(InstallError::Bundle)?;
    if !is_safe_id(manifest.id.as_str()) {
        return Err(InstallError::BadId);
    }
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use feos_archive::ZipWriter;

    fn package(id: &str) -> std::vec::Vec<u8> {
        let mut out = std::vec![0u8; 4096];
        let mut writer = ZipWriter::new(&mut out);
        let manifest = std::format!("id: {id}\nname: Demo\nversion: \"1\"\nentry: ps\n");
        writer.write_stored("manifest.yaml", manifest.as_bytes()).unwrap();
        let len = writer.finish().unwrap();
        out.truncate(len);
        out
    }

    fn digest(data: &[u8]) -> std::string::String {
        hex(&sha256(data))
    }

    #[test]
    fn a_matching_package_is_accepted() {
        let data = package("demo");
        let manifest = verify_package(&data, &digest(&data)).unwrap();
        assert_eq!(manifest.id.as_str(), "demo");
        assert_eq!(verify_package(&data, &digest(&data).to_uppercase()).unwrap().id.as_str(), "demo");
    }

    #[test]
    fn a_changed_byte_or_a_bad_digest_is_refused() {
        let mut data = package("demo");
        let good = digest(&data);
        assert_eq!(verify_package(&data, "abc").unwrap_err(), InstallError::BadDigest);
        data[40] ^= 1;
        assert_eq!(verify_package(&data, &good).unwrap_err(), InstallError::DigestMismatch);
    }

    #[test]
    fn junk_with_the_right_digest_is_still_junk() {
        let data = std::vec![7u8; 1000];
        assert!(matches!(
            verify_package(&data, &digest(&data)),
            Err(InstallError::Bundle(_))
        ));
        let huge = std::vec![0u8; MAX_PACKAGE_BYTES + 1];
        assert_eq!(verify_package(&huge, &digest(&[])).unwrap_err(), InstallError::TooLarge);
    }

    #[test]
    fn an_id_can_not_choose_the_path() {
        for bad in ["../evil", "a/b", ".hidden", "..", "a b"] {
            assert!(!is_safe_id(bad), "{bad}");
        }
        assert!(is_safe_id("media-player_2.0"));
        let data = package("\"../evil\"");
        assert_eq!(verify_package(&data, &digest(&data)).unwrap_err(), InstallError::BadId);
    }

    #[test]
    fn digest_files_are_understood() {
        let digest = "a".repeat(64);
        assert_eq!(parse_digest(&digest), Some(digest.as_str()));
        let line = std::format!("{digest}  demo.fapp\n");
        assert_eq!(parse_digest(&line), Some(digest.as_str()));
        assert_eq!(parse_digest("junk"), None);
        assert_eq!(parse_digest(""), None);
    }
}
