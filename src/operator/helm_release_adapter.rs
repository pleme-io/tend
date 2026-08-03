//! `LockFormat` impl for FluxCD `HelmRelease` YAML files.
//!
//! Each `HelmRelease` resource is a "pin": chart name + version +
//! source ref + (optionally) container image tags surfaced in
//! `spec.values`. One YAML file may contain multiple HelmReleases
//! (and other kinds — Namespace, Secret, etc.) via YAML document
//! separators; the adapter parses the whole file, returns a map
//! keyed by `<namespace>/<release_name>`, ignores non-HelmRelease
//! documents, and on write_pin rewrites only the targeted release's
//! `version` (and matching `image.tag` paths) leaving everything else
//! byte-for-byte untouched.
//!
//! Architectural validation: this is the second `LockFormat` impl
//! after `FlakeLockAdapter`. If both fit cleanly under the trait
//! the abstraction generalizes; if not, the trait needs reshaping.
//! As of this commit: trait fits, modulo edges() (chart sourceRef
//! chains span files, so single-file parse can't see them — see
//! note on edges below).

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_yaml_ng::Value as Yaml;
use std::collections::BTreeMap;
use std::path::Path;

use super::crds::{HelmRev, HelmSourceRef};
use super::lock_format::LockFormat;

pub struct HelmReleaseAdapter;

impl LockFormat for HelmReleaseAdapter {
    type Pin = HelmRev;

    fn lock_path(&self) -> &'static Path {
        // HelmReleases live under cluster overlays at varying depths
        // — there's no single canonical lock_path. The operator's
        // domain orchestrator walks `clusters/*/infrastructure/*/release.yaml`
        // and similar globs, calling parse() on each file's content.
        // Returning "release.yaml" as the conventional filename.
        Path::new("release.yaml")
    }

    fn parse(&self, contents: &str) -> Result<BTreeMap<String, HelmRev>> {
        let mut out = BTreeMap::new();
        for doc in serde_yaml_ng::Deserializer::from_str(contents) {
            let value =
                Yaml::deserialize(doc).context("parsing YAML document in HelmRelease file")?;
            let Some(release) = parse_helmrelease(&value)? else {
                continue;
            };
            let key = format!("{}/{}", release.namespace, release.name);
            out.insert(key, release.pin);
        }
        Ok(out)
    }

    /// HelmRelease dependencies are cross-file (a chart's `sourceRef`
    /// points at a HelmRepository defined elsewhere; an umbrella
    /// chart's sub-chart pin is internal to the chart, not visible
    /// in the HelmRelease YAML). Single-file parse can't observe
    /// these edges. The domain orchestrator that owns directory
    /// walking is the natural place to compute cross-file edges
    /// (HelmRelease → HelmRepository → upstream OCI registry).
    /// For now: empty.
    fn edges(&self, _parsed: &BTreeMap<String, HelmRev>) -> Vec<(String, String)> {
        Vec::new()
    }

    fn write_pin(&self, contents: &str, target: &str, new: &HelmRev) -> Result<String> {
        // Strategy: parse all documents, mutate the matching
        // HelmRelease's spec.chart.spec.version + image.tag entries
        // in spec.values, serialize each document back, rejoin with
        // `---\n`. serde_yaml_ng preserves key order; whitespace and
        // comments do NOT survive the round trip — caller should
        // accept this tradeoff or use a YAML preserver.
        let docs: Vec<Yaml> = serde_yaml_ng::Deserializer::from_str(contents)
            .map(|d| Yaml::deserialize(d).map_err(anyhow::Error::from))
            .collect::<Result<_>>()?;

        let mut found = false;
        let mut updated_docs = Vec::with_capacity(docs.len());
        for mut doc in docs {
            if matches_helmrelease(&doc, target) {
                apply_pin_to_release(&mut doc, new)
                    .with_context(|| format!("applying pin to `{target}`"))?;
                found = true;
            }
            updated_docs.push(doc);
        }

        if !found {
            return Err(anyhow!("no HelmRelease named `{target}` in file"));
        }

        let mut out = String::new();
        for (i, doc) in updated_docs.iter().enumerate() {
            if i > 0 || contents.trim_start().starts_with("---") {
                out.push_str("---\n");
            }
            out.push_str(&serde_yaml_ng::to_string(doc)?);
        }
        if !out.ends_with('\n') {
            out.push('\n');
        }
        Ok(out)
    }
}

// ─── Internals ──────────────────────────────────────────────────────

struct ParsedHelmRelease {
    namespace: String,
    name: String,
    pin: HelmRev,
}

fn parse_helmrelease(value: &Yaml) -> Result<Option<ParsedHelmRelease>> {
    let Yaml::Mapping(map) = value else {
        return Ok(None);
    };
    let api_version = map.get("apiVersion").and_then(|v| v.as_str()).unwrap_or("");
    let kind = map.get("kind").and_then(|v| v.as_str()).unwrap_or("");
    if !api_version.starts_with("helm.toolkit.fluxcd.io/") || kind != "HelmRelease" {
        return Ok(None);
    }

    let metadata = map.get("metadata").and_then(|v| v.as_mapping());
    let name = metadata
        .and_then(|m| m.get("name").and_then(|v| v.as_str()))
        .ok_or_else(|| anyhow!("HelmRelease missing metadata.name"))?
        .to_string();
    let namespace = metadata
        .and_then(|m| m.get("namespace").and_then(|v| v.as_str()))
        .unwrap_or("default")
        .to_string();

    let spec = map.get("spec").and_then(|v| v.as_mapping());
    let chart_spec = spec
        .and_then(|s| s.get("chart").and_then(|c| c.as_mapping()))
        .and_then(|c| c.get("spec").and_then(|s| s.as_mapping()));
    let chart = chart_spec
        .and_then(|c| c.get("chart").and_then(|v| v.as_str()))
        .ok_or_else(|| anyhow!("HelmRelease `{name}` missing spec.chart.spec.chart"))?
        .to_string();
    let version = chart_spec
        .and_then(|c| c.get("version").and_then(|v| v.as_str()))
        .unwrap_or("*")
        .to_string();
    let source_ref_map = chart_spec
        .and_then(|c| c.get("sourceRef").and_then(|v| v.as_mapping()))
        .ok_or_else(|| anyhow!("HelmRelease `{name}` missing spec.chart.spec.sourceRef"))?;
    let source_ref = HelmSourceRef {
        kind: source_ref_map
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("HelmRepository")
            .to_string(),
        name: source_ref_map
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("sourceRef missing name"))?
            .to_string(),
        namespace: source_ref_map
            .get("namespace")
            .and_then(|v| v.as_str())
            .map(String::from),
    };

    let mut image_tags = BTreeMap::new();
    if let Some(values) = spec.and_then(|s| s.get("values")) {
        collect_image_tags(values, "values", &mut image_tags);
    }

    Ok(Some(ParsedHelmRelease {
        namespace,
        name,
        pin: HelmRev {
            chart,
            version,
            source_ref,
            image_tags,
        },
    }))
}

fn collect_image_tags(value: &Yaml, path: &str, out: &mut BTreeMap<String, String>) {
    let Yaml::Mapping(map) = value else { return };
    for (k, v) in map {
        let Some(key) = k.as_str() else { continue };
        let next_path = format!("{path}.{key}");
        // An `image:` mapping containing a `tag:` is the canonical pin
        // location. Ignore image strings of the form `repo/name:tag`
        // (rarely used in pleme-io charts; if present, write_pin won't
        // touch them).
        if key == "image" {
            if let Yaml::Mapping(image) = v {
                if let Some(tag) = image.get("tag").and_then(|t| t.as_str()) {
                    out.insert(format!("{next_path}.tag"), tag.to_string());
                }
            }
        }
        collect_image_tags(v, &next_path, out);
    }
}

fn matches_helmrelease(value: &Yaml, target: &str) -> bool {
    let Yaml::Mapping(map) = value else {
        return false;
    };
    let kind_ok = map.get("kind").and_then(|v| v.as_str()) == Some("HelmRelease");
    if !kind_ok {
        return false;
    }
    let metadata = map.get("metadata").and_then(|v| v.as_mapping());
    let name = metadata
        .and_then(|m| m.get("name").and_then(|v| v.as_str()))
        .unwrap_or("");
    let namespace = metadata
        .and_then(|m| m.get("namespace").and_then(|v| v.as_str()))
        .unwrap_or("default");
    target == format!("{namespace}/{name}")
}

fn apply_pin_to_release(value: &mut Yaml, new: &HelmRev) -> Result<()> {
    let Yaml::Mapping(map) = value else {
        return Err(anyhow!("HelmRelease is not a YAML mapping"));
    };
    let spec = map
        .get_mut("spec")
        .and_then(|v| v.as_mapping_mut())
        .ok_or_else(|| anyhow!("HelmRelease missing spec"))?;
    let chart_spec = spec
        .get_mut("chart")
        .and_then(|v| v.as_mapping_mut())
        .and_then(|c| c.get_mut("spec"))
        .and_then(|s| s.as_mapping_mut())
        .ok_or_else(|| anyhow!("HelmRelease missing spec.chart.spec"))?;
    chart_spec.insert("version".into(), Yaml::from(new.version.clone()));

    if let Some(values) = spec.get_mut("values") {
        for (path, new_tag) in &new.image_tags {
            // path is e.g. "values.image.tag" — strip the "values." prefix
            // and walk dotted segments.
            let rel = path.strip_prefix("values.").unwrap_or(path);
            update_nested_string(values, rel, new_tag);
        }
    }
    Ok(())
}

fn update_nested_string(value: &mut Yaml, dotted_path: &str, new_value: &str) {
    let mut cursor = value;
    let segments: Vec<&str> = dotted_path.split('.').collect();
    let last_idx = segments.len() - 1;
    for (i, seg) in segments.iter().enumerate() {
        let Yaml::Mapping(map) = cursor else { return };
        if i == last_idx {
            map.insert(Yaml::from(*seg), Yaml::from(new_value.to_string()));
            return;
        }
        match map.get_mut(*seg) {
            Some(next) => cursor = next,
            None => return,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
apiVersion: v1
kind: Namespace
metadata:
  name: monitoring
---
apiVersion: helm.toolkit.fluxcd.io/v2
kind: HelmRelease
metadata:
  name: lareira-vm-stack
  namespace: monitoring
spec:
  interval: 30m
  chart:
    spec:
      chart: lareira-vm-stack
      version: "0.1.x"
      sourceRef:
        kind: HelmRepository
        name: pleme-charts
        namespace: flux-system
  values:
    grafana:
      enabled: true
      image:
        repository: grafana/grafana
        tag: "11.2.0"
"#;

    #[test]
    fn parses_helmrelease_pin() {
        let pins = HelmReleaseAdapter.parse(SAMPLE).unwrap();
        assert_eq!(pins.len(), 1);
        let pin = &pins["monitoring/lareira-vm-stack"];
        assert_eq!(pin.chart, "lareira-vm-stack");
        assert_eq!(pin.version, "0.1.x");
        assert_eq!(pin.source_ref.kind, "HelmRepository");
        assert_eq!(pin.source_ref.name, "pleme-charts");
        assert_eq!(pin.source_ref.namespace.as_deref(), Some("flux-system"));
    }

    #[test]
    fn collects_image_tags_from_values() {
        let pins = HelmReleaseAdapter.parse(SAMPLE).unwrap();
        let pin = &pins["monitoring/lareira-vm-stack"];
        assert_eq!(
            pin.image_tags.get("values.grafana.image.tag").unwrap(),
            "11.2.0"
        );
    }

    #[test]
    fn ignores_non_helmrelease_documents() {
        // Namespace doc shouldn't show up as a pin.
        let pins = HelmReleaseAdapter.parse(SAMPLE).unwrap();
        assert!(!pins.contains_key("monitoring/monitoring"));
    }

    #[test]
    fn write_pin_updates_version() {
        let new = HelmRev {
            chart: "lareira-vm-stack".into(),
            version: "0.2.0".into(),
            source_ref: HelmSourceRef {
                kind: "HelmRepository".into(),
                name: "pleme-charts".into(),
                namespace: Some("flux-system".into()),
            },
            image_tags: BTreeMap::new(),
        };
        let updated = HelmReleaseAdapter
            .write_pin(SAMPLE, "monitoring/lareira-vm-stack", &new)
            .unwrap();
        assert!(updated.contains("version: 0.2.0"));
        // Untouched fields preserved (chart name, sourceRef stay).
        assert!(updated.contains("chart: lareira-vm-stack"));
        assert!(updated.contains("name: pleme-charts"));
    }

    #[test]
    fn write_pin_updates_image_tag() {
        let mut tags = BTreeMap::new();
        tags.insert("values.grafana.image.tag".to_string(), "11.4.0".to_string());
        let new = HelmRev {
            chart: "lareira-vm-stack".into(),
            version: "0.1.x".into(),
            source_ref: HelmSourceRef {
                kind: "HelmRepository".into(),
                name: "pleme-charts".into(),
                namespace: Some("flux-system".into()),
            },
            image_tags: tags,
        };
        let updated = HelmReleaseAdapter
            .write_pin(SAMPLE, "monitoring/lareira-vm-stack", &new)
            .unwrap();
        assert!(updated.contains("tag: 11.4.0"));
        assert!(!updated.contains("tag: 11.2.0"));
    }

    #[test]
    fn write_pin_unknown_target_errors() {
        let new = HelmRev {
            chart: "x".into(),
            version: "1.0.0".into(),
            source_ref: HelmSourceRef {
                kind: "HelmRepository".into(),
                name: "y".into(),
                namespace: None,
            },
            image_tags: BTreeMap::new(),
        };
        let err = HelmReleaseAdapter
            .write_pin(SAMPLE, "default/nonexistent", &new)
            .unwrap_err();
        assert!(err.to_string().contains("no HelmRelease named"));
    }

    /// Smoke against the real rio HelmRelease for vm-stack — skipped
    /// when the path doesn't exist (CI / clean checkouts won't have it).
    #[test]
    fn smoke_real_rio_vm_stack_release() {
        let home = std::env::var("HOME").unwrap_or_default();
        let path = std::path::PathBuf::from(home)
            .join("code/github/pleme-io/k8s/clusters/rio/infrastructure/victoria-metrics-k8s-stack/release.yaml");
        if !path.exists() {
            eprintln!("smoke skipped: {} does not exist", path.display());
            return;
        }
        let contents = std::fs::read_to_string(&path).unwrap();
        let pins = HelmReleaseAdapter
            .parse(&contents)
            .expect("parse real release.yaml");
        assert!(
            !pins.is_empty(),
            "expected ≥1 HelmRelease in vm-stack release.yaml"
        );
        let (key, pin) = pins.iter().next().unwrap();
        assert!(key.contains("/"), "key should be ns/name, got `{key}`");
        assert!(!pin.chart.is_empty());
        assert!(!pin.version.is_empty());
    }
}
