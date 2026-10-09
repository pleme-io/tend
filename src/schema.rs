//! tend's config as JSON Schema — the one description of the config's shape
//! that anything outside this crate reads.
//!
//! `schema/tend-config.schema.json` is this module's output, committed and
//! golden-tested ([`tests::committed_schema_is_current`]), and exported by
//! the flake as `lib.configSchema`. The nix repo generates the
//! blackmatter-tend home-manager options from it with substrate's
//! `jsonSchema.optionsFromJsonSchema` (substrate `lib/types/json-schema.nix`)
//! instead of restating every option by hand.
//!
//! That converter REFUSES keywords it cannot map faithfully (`pattern`,
//! `minLength`/`maxLength`, tuples, `not`, conditionals, …) rather than
//! widening them silently. [`tests::schema_uses_only_what_substrate_maps`]
//! holds tend's schema to that vocabulary, so a field that would break the
//! generated module fails here first.
//!
//! Regenerate after a config change: `BLESS=1 cargo test schema` (or
//! `tend config-schema > schema/tend-config.schema.json`).

use crate::config::Config;

/// The committed schema's path, relative to the crate root.
#[cfg(test)]
const COMMITTED: &str = "schema/tend-config.schema.json";

/// The JSON Schema (draft 2020-12, schemars 1.x) of [`Config`].
pub(crate) fn config_schema() -> serde_json::Value {
    serde_json::to_value(schemars1::schema_for!(Config)).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rendered() -> String {
        let mut s = serde_json::to_string_pretty(&config_schema()).unwrap();
        s.push('\n');
        s
    }

    /// Fails when `src/config.rs` (or anything it embeds) changed shape
    /// without the committed schema following. `BLESS=1` rewrites it.
    #[test]
    fn committed_schema_is_current() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(COMMITTED);
        let want = rendered();
        if std::env::var_os("BLESS").is_some() {
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, &want).unwrap();
            return;
        }
        let have = std::fs::read_to_string(&path).unwrap_or_default();
        assert!(
            have == want,
            "{COMMITTED} is stale — regenerate with `BLESS=1 cargo test schema`"
        );
    }

    /// Keywords substrate's `optionsFromJsonSchema` throws on (see the header
    /// of substrate `lib/types/json-schema.nix`).
    const REFUSED: &[&str] = &[
        "pattern",
        "minLength",
        "maxLength",
        "prefixItems",
        "patternProperties",
        "propertyNames",
        "minProperties",
        "maxProperties",
        "not",
        "if",
        "then",
        "else",
        "dependentSchemas",
        "dependentRequired",
        "unevaluatedProperties",
        "unevaluatedItems",
        "contains",
    ];

    fn walk(v: &serde_json::Value, at: &str, bad: &mut Vec<String>) {
        match v {
            serde_json::Value::Object(map) => {
                for (k, child) in map {
                    // Property NAMES are data, not keywords.
                    if at.ends_with("/properties") || at.ends_with("/$defs") {
                        walk(child, &format!("{at}/{k}"), bad);
                        continue;
                    }
                    if REFUSED.contains(&k.as_str()) {
                        bad.push(format!("{at}/{k}"));
                    }
                    if k == "allOf" && child.as_array().is_some_and(|a| a.len() > 1) {
                        bad.push(format!("{at}/allOf (more than one member)"));
                    }
                    if k == "$ref" && !child.as_str().is_some_and(|r| r.starts_with('#')) {
                        bad.push(format!("{at}/$ref (non-local)"));
                    }
                    walk(child, &format!("{at}/{k}"), bad);
                }
            }
            serde_json::Value::Array(items) => {
                for (i, child) in items.iter().enumerate() {
                    walk(child, &format!("{at}/{i}"), bad);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn schema_uses_only_what_substrate_maps() {
        let mut bad = Vec::new();
        walk(&config_schema(), "#", &mut bad);
        assert!(bad.is_empty(), "keywords substrate refuses: {bad:#?}");
    }

    /// The schema states the real defaults — what the generated Nix options
    /// default to — not a second copy that could drift.
    #[test]
    fn schema_carries_the_prescribed_defaults() {
        let schema = config_schema();
        let defs = &schema["$defs"];
        assert_eq!(
            defs["DaemonConfig"]["properties"]["interval"]["default"],
            300
        );
        let auth = &schema["properties"]["github_auth"];
        assert!(
            auth.to_string().contains("TEND_GITHUB_TOKEN"),
            "github_auth default is the shikumi chain: {auth}"
        );
    }
}
