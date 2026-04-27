//! Read-only `flake.nix` parser via rnix (pure Rust, rowan CST).
//!
//! Why this exists: discovery iterates `flake.lock`'s root inputs, but
//! flake.lock can hold stale entries (an input the user removed from
//! `flake.nix` but `nix flake update` hasn't pruned yet). Generating
//! proposals for those stale entries wastes GitHub API quota and
//! creates "ghost" Failed proposals that never apply because
//! `nix flake update <name>` rejects unknown inputs.
//!
//! `read_flake_inputs` walks the rnix CST and returns only the
//! input names the user actually declared in `flake.nix`. The
//! discovery layer intersects this with the lock's root inputs to
//! produce the canonical "actually-bumpable" set.
//!
//! This module is read-only by design. Mutating flake.nix from the
//! operator would be a much larger surface (preserving comments,
//! formatter integration, etc.) — out of scope for the fleet
//! controller, which only writes flake.lock.

use anyhow::{anyhow, Context, Result};
use rnix::ast::{self, Attr, Entry, Expr, HasEntry};
use rowan::ast::AstNode;
use std::collections::BTreeSet;
use std::path::Path;

/// Names of inputs declared in `flake.nix`'s `inputs = { ... }` block.
/// Both the nested form (`inputs = { fenix = { url = "..."; }; };`)
/// and the dotted form (`inputs.fenix.url = "...";`) are recognized.
pub fn read_flake_inputs(path: &Path) -> Result<BTreeSet<String>> {
    let src = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    parse_flake_inputs(&src)
}

/// Pure parsing entry point — no I/O, easier to test in isolation.
pub fn parse_flake_inputs(src: &str) -> Result<BTreeSet<String>> {
    let parse = rnix::Root::parse(src);
    if !parse.errors().is_empty() {
        return Err(anyhow!(
            "flake.nix parse errors: {}",
            parse.errors()
                .iter()
                .map(|e| e.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    let root_expr = parse
        .tree()
        .expr()
        .ok_or_else(|| anyhow!("flake.nix has no root expression"))?;
    let top = match root_expr {
        Expr::AttrSet(s) => s,
        // Some flakes wrap in `let ... in { ... }` — descend through that.
        Expr::LetIn(letin) => match letin.body() {
            Some(Expr::AttrSet(s)) => s,
            _ => return Err(anyhow!("flake.nix root not a recognized attrset shape")),
        },
        _ => return Err(anyhow!("flake.nix root is not an attrset")),
    };

    let mut out = BTreeSet::new();
    for entry in top.entries() {
        let Entry::AttrpathValue(av) = entry else { continue };
        let Some(attrpath) = av.attrpath() else { continue };
        let path: Vec<String> = attrpath_segments(&attrpath);
        if path.first().map(String::as_str) != Some("inputs") {
            continue;
        }
        match path.len() {
            1 => {
                // inputs = { fenix = { url = ...; }; ... };
                if let Some(Expr::AttrSet(inputs)) = av.value() {
                    for e in inputs.entries() {
                        let Entry::AttrpathValue(iav) = e else { continue };
                        let Some(p) = iav.attrpath() else { continue };
                        if let Some(name) = attrpath_segments(&p).into_iter().next() {
                            out.insert(name);
                        }
                    }
                }
            }
            n if n >= 2 => {
                // inputs.fenix.url = "..." (or inputs.fenix = { ... })
                out.insert(path[1].clone());
            }
            _ => {}
        }
    }
    Ok(out)
}

/// Split `inputs.fenix.url` into ["inputs", "fenix", "url"]. Quoted
/// keys (`inputs."fenix-2"`) are unwrapped to the literal name.
fn attrpath_segments(p: &ast::Attrpath) -> Vec<String> {
    p.attrs()
        .filter_map(|a| match a {
            Attr::Ident(i) => i.ident_token().map(|t| t.text().to_string()),
            Attr::Str(s) => {
                // Static-string attr key: e.g. `inputs."fenix-2".url`.
                let raw = s.syntax().text().to_string();
                Some(raw.trim_matches('"').to_string())
            }
            // Dynamic interpolated keys can't be statically resolved;
            // operator can't reason about them so they're excluded.
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_inputs_form() {
        let src = r#"
{
  description = "foo";
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };
  outputs = { self, ... }: {};
}
"#;
        let inputs = parse_flake_inputs(src).unwrap();
        assert!(inputs.contains("nixpkgs"));
        assert!(inputs.contains("fenix"));
    }

    #[test]
    fn dotted_inputs_form() {
        let src = r#"
{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs";
  inputs.substrate.url = "github:pleme-io/substrate";
  inputs.substrate.inputs.nixpkgs.follows = "nixpkgs";
  outputs = { self, ... }: {};
}
"#;
        let inputs = parse_flake_inputs(src).unwrap();
        assert!(inputs.contains("nixpkgs"));
        assert!(inputs.contains("substrate"));
        assert_eq!(inputs.len(), 2, "got {inputs:?}");
    }

    #[test]
    fn mixed_form() {
        let src = r#"
{
  inputs.nixpkgs.url = "github:NixOS/nixpkgs";
  inputs = {
    fenix.url = "github:nix-community/fenix";
  };
  outputs = { self, ... }: {};
}
"#;
        let inputs = parse_flake_inputs(src).unwrap();
        assert!(inputs.contains("nixpkgs"));
        assert!(inputs.contains("fenix"));
    }

    #[test]
    fn quoted_attr_key() {
        let src = r#"
{
  inputs."with-dashes".url = "github:foo/bar";
  outputs = { self, ... }: {};
}
"#;
        let inputs = parse_flake_inputs(src).unwrap();
        assert!(inputs.contains("with-dashes"), "got {inputs:?}");
    }

    #[test]
    fn empty_flake_returns_empty_set() {
        let src = r#"
{
  description = "no inputs";
  outputs = { self, ... }: {};
}
"#;
        let inputs = parse_flake_inputs(src).unwrap();
        assert!(inputs.is_empty());
    }

    #[test]
    fn malformed_flake_errors_explicitly() {
        let src = "{ inputs = ";
        let err = parse_flake_inputs(src).unwrap_err();
        assert!(err.to_string().contains("parse error") || err.to_string().contains("parse errors"));
    }

    /// Smoke test against the real pleme-io/nix flake.nix when present
    /// — skipped on CI / clean checkouts that don't have it.
    #[test]
    fn smoke_real_pleme_io_nix_flake() {
        let home = std::env::var("HOME").unwrap_or_default();
        let path = std::path::PathBuf::from(home).join("code/github/pleme-io/nix/flake.nix");
        if !path.exists() {
            eprintln!("smoke skipped: {} does not exist", path.display());
            return;
        }
        let inputs = read_flake_inputs(&path).expect("parse real flake.nix");
        // The pleme-io nix repo declares many inputs; we just assert it
        // produces a non-empty set with at least nixpkgs (every flake
        // in the fleet has nixpkgs).
        assert!(
            !inputs.is_empty(),
            "expected non-empty input set, got {inputs:?}"
        );
        assert!(
            inputs.contains("nixpkgs") || inputs.contains("nixpkgs-unstable"),
            "expected nixpkgs in inputs, got {inputs:?}"
        );
    }
}
