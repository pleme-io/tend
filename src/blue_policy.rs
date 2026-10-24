//! The one way tend loads a blue program it hosts: the standard bidamas,
//! a pure frame (every host-effect name unbound, so a policy that reads the
//! clock or the disk does not compile) and the tree-walker budget.
//!
//! `blue/align.b`, `blue/cargo_target.b`, `blue/drift.b` and an operator's
//! `tend.b` load here; Rust hands them facts and decodes their answer.

use std::path::Path;

use anyhow::{anyhow, Result};
use blue_lang_host::{Budget, Entry, Program, Standard};
use serde_json::Value as Json;

pub(crate) fn load_pure(path: Option<&Path>, text: &str, shown: &str) -> Result<Program<()>> {
    let effects = blue_lang_host::host_effect_names();
    let pure = |name: &str| !effects.contains(&name);
    Program::<()>::builder(Entry { path, text })
        .loader(&Standard)
        .frame(&pure, Budget::tree_walker_default())
        .load(&mut ())
        .map_err(|e| anyhow!("{shown}: {e}"))
}

pub(crate) fn call(text: &str, shown: &str, name: &str, args: &[Json]) -> Result<Json> {
    let mut program = load_pure(None, text, shown)?;
    let values: Vec<blue_lang_host::Value> = args
        .iter()
        .map(blue_lang_runtime::json::json_to_maps)
        .collect();
    let out: blue_lang_host::Value = program
        .call(name, values, &mut ())
        .map_err(|e| anyhow!("{shown}: {name}(): {e}"))?;
    Ok(blue_lang_runtime::json::value_to_json(&out))
}
