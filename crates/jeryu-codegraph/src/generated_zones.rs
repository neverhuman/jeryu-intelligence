//! Shared parser for a repo's `agent/generated-zones.toml`.
//!
//! The family standard (what jankurai reads) is `[[zone]]` tables with
//! `path`, `source`, `command`, `read_only` and `write_policy`. Older files
//! use a `[[zones]]` array with `generator` and `manual_edits`; both shapes
//! load into the same [`GeneratedZoneSpec`].

use serde::Deserialize;

/// One generated zone, normalised from either file shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GeneratedZoneSpec {
    /// Zone path or glob.
    pub(crate) path: String,
    /// Command (or source) that regenerates the zone; empty when unspecified.
    pub(crate) generator: String,
    /// Whether hand edits are permitted.
    pub(crate) manual_edits: bool,
}

#[derive(Debug, Default, Deserialize)]
struct ZonesFile {
    #[serde(default)]
    zone: Vec<RawZone>,
    #[serde(default)]
    zones: Vec<RawZone>,
}

#[derive(Debug, Deserialize)]
struct RawZone {
    path: String,
    generator: Option<String>,
    command: Option<String>,
    source: Option<String>,
    manual_edits: Option<bool>,
    read_only: Option<bool>,
}

impl RawZone {
    fn into_spec(self) -> GeneratedZoneSpec {
        // Generated output is not hand-edited unless the file says so.
        let manual_edits = self
            .manual_edits
            .unwrap_or_else(|| !self.read_only.unwrap_or(true));
        let generator = self
            .generator
            .or(self.command)
            .or(self.source)
            .unwrap_or_default();
        GeneratedZoneSpec {
            path: self.path,
            generator,
            manual_edits,
        }
    }
}

/// Parse `agent/generated-zones.toml` text, accepting `[[zone]]` and `[[zones]]`.
pub(crate) fn parse_generated_zones(text: &str) -> Result<Vec<GeneratedZoneSpec>, toml::de::Error> {
    let file: ZonesFile = toml::from_str(text)?;
    Ok(file
        .zone
        .into_iter()
        .chain(file.zones)
        .map(RawZone::into_spec)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAMILY_STANDARD: &str = r#"
[[zone]]
path = "apps/web/dist/"
source = "npm --workspace @jain/web run build"
command = "npm run build"
read_only = true
write_policy = "generated"

[[zone]]
path = "contracts/generated/**"
source = "contracts/openapi.yaml"
read_only = false
write_policy = "review"
"#;

    #[test]
    fn parses_family_standard_zone_tables() {
        let zones = parse_generated_zones(FAMILY_STANDARD).unwrap();
        assert_eq!(zones.len(), 2);
        assert_eq!(zones[0].path, "apps/web/dist/");
        assert_eq!(zones[0].generator, "npm run build");
        assert!(!zones[0].manual_edits);
        assert_eq!(zones[1].generator, "contracts/openapi.yaml");
        assert!(zones[1].manual_edits);
    }

    #[test]
    fn still_accepts_zones_array() {
        let zones = parse_generated_zones(
            "[[zones]]\npath = \"gen/\"\ngenerator = \"just gen\"\nmanual_edits = true\n",
        )
        .unwrap();
        assert_eq!(zones.len(), 1);
        assert_eq!(zones[0].generator, "just gen");
        assert!(zones[0].manual_edits);
    }

    #[test]
    fn path_only_zone_is_read_only() {
        let zones = parse_generated_zones("[[zone]]\npath = \"x/**\"\n").unwrap();
        assert!(!zones[0].manual_edits);
        assert_eq!(zones[0].generator, "");
    }
}
