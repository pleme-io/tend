//! The one way tend loads a blue program it hosts: the standard bidamas,
//! a pure frame (every host-effect name unbound, so a policy that reads the
//! clock or the disk does not compile) and the tree-walker budget.
//!
//! `blue/align.b`, `blue/cargo_target.b` and an operator's `tend.b` load
//! here; Rust hands them facts and decodes their answer.

use std::path::Path;

use anyhow::{anyhow, Result};
use blue_lang_host::{Budget, Entry, Program, Standard};

pub(crate) fn load_pure(path: Option<&Path>, text: &str, shown: &str) -> Result<Program<()>> {
    let effects = blue_lang_host::host_effect_names();
    let pure = |name: &str| !effects.contains(&name);
    Program::<()>::builder(Entry { path, text })
        .loader(&Standard)
        .frame(&pure, Budget::tree_walker_default())
        .load(&mut ())
        .map_err(|e| anyhow!("{shown}: {e}"))
}
