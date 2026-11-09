//! Bundle manifests.

use feos_text::Text;
use feos_yaml::{NodeId, YamlDocument, YamlNode};

use crate::{BundleError, Result};

/// Maximum accepted manifest size.
pub const MANIFEST_MAX: usize = 4096;

/// Metadata read from a bundle's `manifest.yaml`.
///
/// All strings are fixed-capacity [`Text`] values. A longer field is rejected
/// with [`BundleError::FieldTooLong`]: truncating an `entry` or `args` would
/// quietly launch something else.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Manifest {
    /// Stable application identifier (`terminal`, `files`, ...).
    pub id: Text<32>,
    /// Display name shown in the start menu and taskbar.
    pub name: Text<48>,
    /// Human-readable version.
    pub version: Text<16>,
    /// Shell registry entry point.
    pub entry: Text<32>,
    /// Start-menu category.
    pub category: Text<24>,
    /// One-line description.
    pub description: Text<96>,
    /// Bundle-relative icon path.
    pub icon: Text<32>,
    /// An exact 32 pixel version of the icon, if the bundle has one.
    pub icon_medium: Text<32>,
    /// An exact 16 pixel version of the icon, if the bundle has one.
    pub icon_small: Text<32>,
    /// Arguments passed to the entry point.
    pub args: Text<64>,
    /// Preferred initial window width.
    pub window_width: u32,
    /// Preferred initial window height.
    pub window_height: u32,
    /// The app draws its own title bar and border (`window.frameless: true`),
    /// so the desktop draws none, and the size is the content's own.
    pub frameless: bool,
}

impl Manifest {
    /// An empty manifest, used to initialise catalog slots.
    pub const EMPTY: Self = Self {
        id: Text::EMPTY,
        name: Text::EMPTY,
        version: Text::EMPTY,
        entry: Text::EMPTY,
        category: Text::EMPTY,
        description: Text::EMPTY,
        icon: Text::EMPTY,
        icon_medium: Text::EMPTY,
        icon_small: Text::EMPTY,
        args: Text::EMPTY,
        window_width: 0,
        window_height: 0,
        frameless: false,
    };

    /// Parses a manifest from YAML.
    pub fn parse(yaml: &str) -> Result<Self> {
        let mut nodes = [YamlNode::EMPTY; 64];
        let document = YamlDocument::parse(yaml, &mut nodes)?;
        let root = document.root();

        if !document.is_mapping(root) {
            return Err(BundleError::MissingField("id"));
        }

        let mut manifest = Self::EMPTY;
        read_text(&document, root, "id", &mut manifest.id)?;
        if manifest.id.is_empty() {
            return Err(BundleError::MissingField("id"));
        }
        read_text(&document, root, "name", &mut manifest.name)?;
        if manifest.name.is_empty() {
            manifest.name.set(manifest.id.as_str());
        }
        read_text(&document, root, "version", &mut manifest.version)?;
        if manifest.version.is_empty() {
            manifest.version.set("1.0.0");
        }
        read_text(&document, root, "entry", &mut manifest.entry)?;
        if manifest.entry.is_empty() {
            manifest.entry.set(manifest.id.as_str());
        }
        read_text(&document, root, "category", &mut manifest.category)?;
        if manifest.category.is_empty() {
            manifest.category.set("Applications");
        }
        read_text(&document, root, "description", &mut manifest.description)?;
        read_text(&document, root, "icon", &mut manifest.icon)?;
        read_text(&document, root, "icon_medium", &mut manifest.icon_medium)?;
        read_text(&document, root, "icon_small", &mut manifest.icon_small)?;
        if manifest.icon.is_empty() {
            manifest.icon.set("icon.bmp");
        }
        read_text(&document, root, "args", &mut manifest.args)?;

        manifest.window_width = 560;
        manifest.window_height = 380;
        if let Some(window) = document.get(root, "window") {
            if let Some(value) = document
                .get(window, "width")
                .and_then(|node| document.as_u64(node))
            {
                manifest.window_width = value.clamp(240, 2048) as u32;
            }
            if let Some(value) = document
                .get(window, "height")
                .and_then(|node| document.as_u64(node))
            {
                manifest.window_height = value.clamp(160, 2048) as u32;
            }
            if let Some(value) = document
                .get(window, "frameless")
                .and_then(|node| document.as_bool(node))
            {
                manifest.frameless = value;
            }
        }

        Ok(manifest)
    }
}

fn read_text<const N: usize>(
    document: &YamlDocument<'_, '_>,
    root: NodeId,
    key: &'static str,
    out: &mut Text<N>,
) -> Result<()> {
    let Some(node) = document.get(root, key) else {
        return Ok(());
    };
    if document.scalar(node).is_none() {
        return Err(BundleError::InvalidField(key));
    }
    // The parser has validated escapes, so a failed decode can only mean the
    // value is longer than any field.
    let mut buffer = [0u8; 256];
    let value = document
        .as_str(node, &mut buffer)
        .ok_or(BundleError::FieldTooLong(key))?;
    if !out.set(value) {
        return Err(BundleError::FieldTooLong(key));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_required_and_optional_fields() {
        let manifest = Manifest::parse(concat!(
            "id: terminal\n",
            "name: Terminal\n",
            "entry: gui\n",
            "category: System\n",
            "description: \"A control center\"\n",
            "icon: icon.bmp\n",
            "window:\n",
            "  width: 640\n",
            "  height: 420\n",
        ))
        .unwrap();

        assert_eq!(manifest.id.as_str(), "terminal");
        assert_eq!(manifest.name.as_str(), "Terminal");
        assert_eq!(manifest.entry.as_str(), "gui");
        assert_eq!(manifest.version.as_str(), "1.0.0");
        assert_eq!(manifest.window_width, 640);
        assert_eq!(manifest.window_height, 420);
        assert!(!manifest.frameless);
    }

    #[test]
    fn a_window_can_ask_for_no_frame() {
        let manifest = Manifest::parse("id: amp\nwindow:\n  width: 275\n  height: 464\n  frameless: true\n").unwrap();
        assert!(manifest.frameless);
        assert_eq!((manifest.window_width, manifest.window_height), (275, 464));
    }

    #[test]
    fn fills_defaults_and_rejects_missing_id() {
        assert!(Manifest::parse("name: Broken\n").is_err());
        let manifest = Manifest::parse("id: demo\n").unwrap();
        assert_eq!(manifest.name.as_str(), "demo");
        assert_eq!(manifest.entry.as_str(), "demo");
        assert_eq!(manifest.category.as_str(), "Applications");
    }

    #[test]
    fn oversized_fields_are_errors_not_defaults() {
        // At 300 bytes this used to fall back to `id`, launching another command.
        let long = "x".repeat(300);
        let yaml = format!("id: demo\nentry: {long}\n");
        assert_eq!(
            Manifest::parse(&yaml),
            Err(BundleError::FieldTooLong("entry"))
        );
        // Just over the field's own capacity used to be truncated.
        let yaml = format!("id: demo\nargs: {}\n", "a".repeat(65));
        assert_eq!(
            Manifest::parse(&yaml),
            Err(BundleError::FieldTooLong("args"))
        );
        let yaml = format!("id: {}\n", "a".repeat(33));
        assert_eq!(Manifest::parse(&yaml), Err(BundleError::FieldTooLong("id")));

        let yaml = format!("id: {}\nargs: {}\n", "a".repeat(32), "b".repeat(64));
        let manifest = Manifest::parse(&yaml).unwrap();
        assert_eq!(manifest.id.len(), 32);
        assert_eq!(manifest.args.len(), 64);
    }

    #[test]
    fn non_scalar_fields_are_errors() {
        assert_eq!(
            Manifest::parse("id: demo\nentry:\n  - a\n"),
            Err(BundleError::InvalidField("entry"))
        );
        assert_eq!(
            Manifest::parse("id: demo\nargs: \"\"\n")
                .unwrap()
                .args
                .as_str(),
            ""
        );
    }
}
