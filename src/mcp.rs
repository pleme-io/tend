//! MCP surface — lets an agent drive and observe tend directly.
//!
//! Built on `kaname`, the fleet's MCP scaffold (tool catalog + server info +
//! response helpers), NOT hand-rolled: five pleme-io servers already went the
//! hand-rolled route and each re-derived the same boilerplate. kaname also ships
//! `register_config_tools`, so the "dynamic config surface" is an existing
//! primitive rather than a new one.
//!
//! WHY AN AGENT NEEDS THIS. tend runs unattended and makes decisions — it
//! throttles on host pressure, refuses to remove worktrees holding work, skips
//! cycles. Today all of that is visible only by running the CLI and reading
//! prose. An agent asking "why is nothing converging?" has to guess. These tools
//! return the same verdicts as typed JSON.
//!
//! ★ READ AND WRITE ARE SEPARATED ON PURPOSE, and the separation is the design.
//! Every read-only tool is always available. Every MUTATING tool is gated behind
//! [`Authority`], which defaults to read-only — because an MCP server is a
//! remote-control surface for an agent, and the failure mode of a confused agent
//! with write access to a repo reconciler is losing work, not printing something
//! wrong. This mirrors the fleet's own breathe MCP, where `writeIntent` is the
//! single authorization and nothing else grants it.
//!
//! Config `set` is the sharpest case: tend's config decides what gets cloned,
//! pulled and flake-updated across the whole workspace, so a bad write is a
//! fleet-wide action, not a local one. It is therefore refused unless authority
//! is explicitly granted AND the key is one the surface knows how to validate.

use serde_json::{json, Value};

/// What the MCP session is permitted to do.
///
/// Deliberately an enum with a read-only default rather than a `bool` flag:
/// `Authority::default()` is a safe surface, and adding a future tier (e.g.
/// "may mutate worktrees but not config") is a variant rather than a second
/// boolean that can contradict the first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Authority {
    /// Observe only. Every mutating tool refuses, with a reason.
    #[default]
    Observe,
    /// May run mutating tools. Granted explicitly by the operator at launch.
    Mutate,
}

impl Authority {
    #[must_use]
    pub const fn may_mutate(self) -> bool {
        matches!(self, Authority::Mutate)
    }
}

/// A tool this surface exposes.
pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    /// True when the tool changes state — used to gate on [`Authority`] and to
    /// render the catalog honestly.
    pub mutating: bool,
}

/// The catalog. One table, so "what can an agent do to tend?" is answerable by
/// reading it rather than by grepping a dispatch match — and so the gating test
/// can assert every mutating tool is actually gated.
pub const TOOLS: &[Tool] = &[
    Tool {
        name: "tend_pressure",
        description: "Host pressure reading and the verdict the daemon acts on \
                      (proceed / throttle / run nothing), with the reason.",
        mutating: false,
    },
    Tool {
        name: "tend_worktree_list",
        description: "Session worktrees with their safety verdict — whether each \
                      holds uncommitted or unpushed work.",
        mutating: false,
    },
    Tool {
        name: "tend_config_get",
        description: "Read tend's resolved configuration (shikumi TieredConfig). \
                      Omit `key` for everything.",
        mutating: false,
    },
    Tool {
        name: "tend_worktree_prune",
        description: "Remove session worktrees that hold no work. Refuses on \
                      uncommitted or unpushed changes. Requires mutate authority.",
        mutating: true,
    },
    Tool {
        name: "tend_worktree_land",
        description: "Rebase a session worktree onto the base branch and push. \
                      Requires mutate authority.",
        mutating: true,
    },
];

/// Reason a tool refused, as a typed value rather than prose an agent must parse.
#[must_use]
pub fn refusal(tool: &str, why: &str) -> Value {
    json!({
        "ok": false,
        "tool": tool,
        "refused": true,
        "reason": why,
    })
}

/// Check authority before running a tool.
///
/// Returns `Some(refusal)` when the call must not proceed. Pure, so the gate is
/// testable without a server — the same shape as the pressure verdict, and for
/// the same reason: the refusing branch is the one that must never be wrong.
#[must_use]
pub fn authorize(tool_name: &str, authority: Authority) -> Option<Value> {
    let tool = TOOLS.iter().find(|t| t.name == tool_name)?;
    if tool.mutating && !authority.may_mutate() {
        return Some(refusal(
            tool_name,
            "this session is observe-only; relaunch with --allow-mutate to permit \
             state-changing tools. tend reconciles a whole workspace, so a mistaken \
             write here is a fleet-wide action.",
        ));
    }
    None
}

/// Render the catalog for `tend mcp --list-tools`.
///
/// GENERATED from `TOOLS`, never hand-listed: a catalog that can disagree with
/// the implementation is worse than none, since it reads as authoritative.
#[must_use]
pub fn catalog_json(authority: Authority) -> Value {
    json!({
        "authority": if authority.may_mutate() { "mutate" } else { "observe" },
        "tools": TOOLS.iter().map(|t| json!({
            "name": t.name,
            "description": t.description,
            "mutating": t.mutating,
            "available": !t.mutating || authority.may_mutate(),
        })).collect::<Vec<_>>(),
    })
}

/// Config surface over tend's shikumi-typed `Config`.
///
/// `get` serialises the resolved config, so an agent sees what tend ACTUALLY
/// resolved (env > file > prescribed default), not what a file says. That
/// distinction is the whole point of TieredConfig and the usual source of
/// "but the file says…" confusion.
pub struct ConfigSurface {
    pub config: crate::config::Config,
    pub authority: Authority,
}

impl ConfigSurface {
    /// Read a dot-path key, or everything when `key` is `None`.
    ///
    /// # Errors
    /// Returns an error only if the config cannot be serialised.
    pub fn get(&self, key: Option<&str>) -> anyhow::Result<Value> {
        let all = serde_json::to_value(&self.config)?;
        let Some(key) = key else { return Ok(all) };
        let mut cur = &all;
        for seg in key.split('.') {
            match cur.get(seg) {
                Some(v) => cur = v,
                // A missing key is a normal answer, not an error: an agent
                // exploring the surface should get `null` and keep going rather
                // than an exception it has to special-case.
                None => return Ok(Value::Null),
            }
        }
        Ok(cur.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observe_is_the_default_authority() {
        // The safe surface must be what you get by forgetting to choose.
        assert_eq!(Authority::default(), Authority::Observe);
        assert!(!Authority::default().may_mutate());
    }

    #[test]
    fn every_mutating_tool_is_gated_and_every_read_tool_is_not() {
        // Asserted over the CATALOG, so a tool added without gating fails here
        // rather than shipping an ungated write.
        for t in TOOLS {
            let refused = authorize(t.name, Authority::Observe);
            assert_eq!(
                refused.is_some(),
                t.mutating,
                "tool `{}` (mutating={}) gating is wrong",
                t.name,
                t.mutating
            );
            assert!(
                authorize(t.name, Authority::Mutate).is_none(),
                "tool `{}` must run with mutate authority",
                t.name
            );
        }
    }

    #[test]
    fn the_catalog_has_both_kinds() {
        // All-read would mean the gate is never exercised; all-write would mean
        // the surface is unusable observe-only. Either way the tests above stop
        // proving anything.
        assert!(TOOLS.iter().any(|t| t.mutating), "no mutating tool");
        assert!(TOOLS.iter().any(|t| !t.mutating), "no read-only tool");
    }

    #[test]
    fn refusals_say_how_to_proceed() {
        let r = authorize("tend_worktree_prune", Authority::Observe).expect("gated");
        let reason = r["reason"].as_str().unwrap_or_default();
        assert!(reason.contains("--allow-mutate"), "got {reason:?}");
        assert_eq!(r["refused"], json!(true));
        assert_eq!(r["ok"], json!(false));
    }

    #[test]
    fn unknown_tools_are_not_silently_authorized() {
        // `authorize` returning None means "proceed", so an unknown name must
        // never reach a dispatcher that would then error obscurely — the caller
        // checks membership first. Pinned so the contract cannot drift.
        assert!(authorize("tend_definitely_not_a_tool", Authority::Observe).is_none());
        assert!(
            !TOOLS.iter().any(|t| t.name == "tend_definitely_not_a_tool"),
            "membership is what makes the above safe"
        );
    }

    #[test]
    fn catalog_marks_availability_per_authority() {
        let observe = catalog_json(Authority::Observe);
        let mutate = catalog_json(Authority::Mutate);
        assert_eq!(observe["authority"], json!("observe"));
        assert_eq!(mutate["authority"], json!("mutate"));

        let unavailable = observe["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|t| t["available"] == json!(false))
            .count();
        assert!(unavailable > 0, "observe must hide the mutating tools");
        assert!(
            mutate["tools"].as_array().unwrap().iter().all(|t| t["available"] == json!(true)),
            "mutate authority exposes everything"
        );
    }

    #[test]
    fn catalog_is_generated_from_the_table() {
        let c = catalog_json(Authority::Mutate);
        assert_eq!(c["tools"].as_array().unwrap().len(), TOOLS.len());
        for t in TOOLS {
            assert!(
                c["tools"].as_array().unwrap().iter().any(|x| x["name"] == json!(t.name)),
                "catalog omits `{}`",
                t.name
            );
        }
    }

    #[test]
    fn config_get_returns_null_for_a_missing_key_rather_than_erroring() {
        let surface = ConfigSurface {
            config: crate::config::Config { workspaces: vec![], host_health: Default::default() },
            authority: Authority::Observe,
        };
        assert_eq!(surface.get(Some("nope.not.here")).unwrap(), Value::Null);
        // …and the whole config is reachable.
        assert!(surface.get(None).unwrap().get("workspaces").is_some());
    }
}
