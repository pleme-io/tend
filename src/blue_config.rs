//! `tend.b`: tend's configuration declared in blue.
//!
//! The discovered configuration file may be `tend.b` instead of YAML (blue is
//! the source; YAML is a rendering, theory/FUEKI.md R9). Blue evaluates it in a
//! pure frame, no clock, file, process, environment or network, under the tree
//! walker's default budget, with the standard bidamas available to `use`. Its
//! `config()` value becomes the file layer with the `.b` path as provenance, so
//! every layer above it (`--config`, `TEND_*`, flags, `--set`) folds over it
//! unchanged and an unknown or ill-typed key is still refused naming its layer.
//!
//! One thing YAML could not do: a workspace entry that does not read as a
//! workspace is refused alone, by name, and its siblings stand. In YAML one bad
//! workspace fails the whole configuration. `TEND_CONFIG_BLUE=off` ignores
//! `tend.b`, and discovery falls back to the YAML.

use std::path::Path;

use anyhow::{anyhow, Context, Result};
use blue_lang_host::Value;

pub(crate) const FILE_NAME: &str = "tend.b";

pub(crate) fn is_blue(path: &Path) -> bool {
    path.extension().is_some_and(|e| e == "b")
}

pub(crate) fn enabled() -> bool {
    std::env::var("TEND_CONFIG_BLUE").map_or(true, |v| v != "off")
}

/// The declaration's value as JSON, and the entries refused on the way.
#[derive(Debug)]
pub(crate) struct Evaluated {
    pub json: serde_json::Value,
    pub refusals: Vec<String>,
}

pub(crate) fn evaluate(path: &Path) -> Result<Evaluated> {
    let text = std::fs::read_to_string(path)
        .with_context(|| ["reading ", &path.display().to_string()].concat())?;
    evaluate_text(Some(path), &text)
}

pub(crate) fn evaluate_text(path: Option<&Path>, text: &str) -> Result<Evaluated> {
    let shown = path.map_or_else(|| "tend.b".to_string(), |p| p.display().to_string());
    let mut program = crate::blue_policy::load_pure(path, text, &shown)?;
    if !program.defines("config") {
        return Err(anyhow!("{shown}: defines no `config()`; it returns the configuration map"));
    }
    let value: Value = program
        .call("config", (), &mut ())
        .map_err(|e| anyhow!("{shown}: config(): {e}"))?;
    Ok(judge_workspaces(blue_lang_runtime::json::value_to_json(&value)))
}

fn judge_workspaces(mut json: serde_json::Value) -> Evaluated {
    let mut refusals = Vec::new();
    if let Some(serde_json::Value::Array(items)) = json.get_mut("workspaces") {
        items.retain(|item| {
            match serde_json::from_value::<crate::config::Workspace>(item.clone()) {
                Ok(_) => true,
                Err(e) => {
                    let name = item.get("name").and_then(|n| n.as_str()).unwrap_or("(unnamed)");
                    refusals.push(["workspace ", name, ": ", &e.to_string()].concat());
                    false
                }
            }
        });
    }
    Evaluated { json, refusals }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TWO: &str = "\
def config()
  {
    workspaces: [
      {name: \"one\", org: \"pleme-io\", base_dir: \"~/code/github/pleme-io\"},
      {name: \"two\", org: \"drzln\", base_dir: 7}
    ],
    daemon: {interval: 120}
  }
end
";

    #[test]
    fn a_bad_workspace_is_refused_alone_and_its_sibling_stands() {
        let out = evaluate_text(None, TWO).unwrap();
        let ws = out.json["workspaces"].as_array().unwrap();
        assert_eq!(ws.len(), 1, "{}", out.json);
        assert_eq!(ws[0]["name"], "one");
        assert_eq!(out.refusals.len(), 1, "{:?}", out.refusals);
        assert!(out.refusals[0].starts_with("workspace two: "), "{:?}", out.refusals);
        assert_eq!(out.json["daemon"]["interval"], 120);
    }

    #[test]
    fn the_declaration_is_computed_in_blue() {
        let src = "\
def org_workspace(org)
  {name: org, org:, base_dir: \"~/code/github/#{org}\"}
end

def config()
  {workspaces: map(org_workspace, %w[pleme-io drzln])}
end
";
        let out = evaluate_text(None, src).unwrap();
        assert!(out.refusals.is_empty(), "{:?}", out.refusals);
        assert_eq!(out.json["workspaces"][1]["base_dir"], "~/code/github/drzln");
    }

    #[test]
    fn a_declaration_without_config_or_with_an_effect_is_refused() {
        let none = evaluate_text(None, "def other()\n  1\nend\n").unwrap_err().to_string();
        assert!(none.contains("defines no `config()`"), "{none}");
        let clock = evaluate_text(None, "def config()\n  {stamp: now_ms}\nend\n")
            .unwrap_err()
            .to_string();
        assert!(clock.contains("unbound name `now_ms`"), "the clock is not in a config's frame: {clock}");
    }

    #[test]
    fn a_tend_b_file_resolves_through_the_config_loader() {
        let dir = std::env::temp_dir().join(["tend-blue-", &std::process::id().to_string()].concat());
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE_NAME);
        std::fs::write(&path, TWO).unwrap();
        let config = crate::config_layers::load_file(&path).unwrap();
        assert_eq!(config.workspaces.len(), 1);
        assert_eq!(config.workspaces[0].name, "one");
        assert_eq!(config.daemon.interval, 120);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn only_a_dot_b_file_is_blue() {
        assert!(is_blue(Path::new("/x/tend.b")));
        assert!(!is_blue(Path::new("/x/tend.yaml")));
    }
}
