use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use blue_lang_host::{Program, Value};
use serde::Serialize;
use serde_json::{json, Value as Json};

use crate::config::Workspace;
use crate::flake_graph::FlakeGraph;
use crate::flake_lock::ExtendedLockFile;

const SOURCE: &str = include_str!("../blue/align.b");

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Facts {
    pub edges: BTreeSet<(String, String)>,
    pub pins: BTreeSet<(String, String, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Skew {
    pub item: String,
    pub target: String,
    pub versions: Vec<(String, Vec<String>)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Move {
    pub wave: usize,
    pub repo: String,
    pub item: String,
    pub from: String,
    pub to: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub(crate) enum Proposal {
    Ok {
        waves: Vec<Vec<String>>,
        skews: Vec<Skew>,
        moves: Vec<Move>,
        blocked: Vec<String>,
    },
    Refused {
        why: String,
    },
}

impl Facts {
    fn edge_json(&self) -> Json {
        Json::Array(self.edges.iter().map(|(a, b)| json!([a, b])).collect())
    }

    fn node_json(&self) -> Json {
        let nodes: BTreeSet<&String> = self.edges.iter().flat_map(|(a, b)| [a, b]).collect();
        Json::Array(nodes.into_iter().map(|n| json!(n)).collect())
    }

    fn pin_json(&self) -> Json {
        Json::Array(
            self.pins
                .iter()
                .map(|(r, i, v)| json!([r, i, v]))
                .collect(),
        )
    }
}

pub(crate) struct Engine(Program<()>);

impl Engine {
    pub(crate) fn new() -> Result<Self> {
        load().map(Self)
    }

    pub(crate) fn propose(&mut self, facts: &Facts) -> Result<Proposal> {
        call(&mut self.0, facts)
    }
}

pub(crate) fn propose(facts: &Facts) -> Result<Proposal> {
    Engine::new()?.propose(facts)
}

fn load() -> Result<Program<()>> {
    crate::blue_policy::load_pure(None, SOURCE, "align.b")
}

fn call(program: &mut Program<()>, facts: &Facts) -> Result<Proposal> {
    let nodes = blue_lang_runtime::json::json_to_value(&facts.node_json());
    let edges = blue_lang_runtime::json::json_to_value(&facts.edge_json());
    let pins = blue_lang_runtime::json::json_to_value(&facts.pin_json());
    let out: Value = program
        .call("propose", (nodes, edges, pins), &mut ())
        .map_err(|e| anyhow!("align.b: propose(): {e}"))?;
    decode(&out)
}

fn text(v: &Value) -> Result<String> {
    match v {
        Value::Str(s) | Value::Symbol(s) | Value::Keyword(s) => Ok(s.as_ref().to_owned()),
        other => Err(anyhow!("expected text, got {other:?}")),
    }
}

fn list(v: &Value) -> Result<Vec<Value>> {
    match v {
        Value::List(xs) => Ok(xs.iter().cloned().collect()),
        other => Err(anyhow!("expected a list, got {other:?}")),
    }
}

fn texts(v: &Value) -> Result<Vec<String>> {
    list(v)?.iter().map(text).collect()
}

fn decode(out: &Value) -> Result<Proposal> {
    let parts = list(out)?;
    match parts.first().map(text).transpose()?.as_deref() {
        Some("ok") => {
            let waves = list(&parts[1])?
                .iter()
                .map(texts)
                .collect::<Result<Vec<_>>>()?;
            let skews = list(&parts[2])?
                .iter()
                .map(|s| {
                    let s = list(s)?;
                    let versions = list(&s[2])?
                        .iter()
                        .map(|vh| {
                            let vh = list(vh)?;
                            Ok((text(&vh[0])?, texts(&vh[1])?))
                        })
                        .collect::<Result<Vec<_>>>()?;
                    Ok(Skew {
                        item: text(&s[0])?,
                        target: text(&s[1])?,
                        versions,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            let moves = list(&parts[3])?
                .iter()
                .map(|m| {
                    let m = list(m)?;
                    let Value::Int(wave) = &m[0] else {
                        return Err(anyhow!("wave is not a number: {:?}", m[0]));
                    };
                    Ok(Move {
                        wave: usize::try_from(*wave)?,
                        repo: text(&m[1])?,
                        item: text(&m[2])?,
                        from: text(&m[3])?,
                        to: text(&m[4])?,
                    })
                })
                .collect::<Result<Vec<_>>>()?;
            Ok(Proposal::Ok {
                waves,
                skews,
                moves,
                blocked: texts(&parts[4])?,
            })
        }
        Some("refused") => Ok(Proposal::Refused {
            why: text(&parts[1])?,
        }),
        _ => Err(anyhow!("align.b returned an unknown verdict: {out:?}")),
    }
}

pub(crate) fn gather(ws: &Workspace) -> Result<Facts> {
    let base = ws.resolved_base_dir()?;
    let org = ws.org.clone().unwrap_or_else(|| ws.name.clone());
    let (graph, _) = FlakeGraph::for_workspace(ws)?;
    gather_at(&base, &org, &graph)
}

pub(crate) fn gather_at(base: &Path, org: &str, graph: &FlakeGraph) -> Result<Facts> {
    let mut facts = Facts::default();
    for (repo, deps) in graph.deps() {
        for dep in deps {
            facts.edges.insert((repo.clone(), dep.clone()));
        }
    }
    let known: BTreeSet<String> = std::fs::read_dir(base)
        .with_context(|| format!("reading {}", base.display()))?
        .flatten()
        .filter(|e| e.path().join(".git").exists())
        .filter_map(|e| e.file_name().to_str().map(str::to_owned))
        .collect();
    for repo in &known {
        let dir = base.join(repo);
        flake_pins(&dir, repo, org, &known, &mut facts);
        cargo_pins(&dir, repo, org, &known, &mut facts);
    }
    Ok(facts)
}

fn flake_pins(dir: &Path, repo: &str, org: &str, known: &BTreeSet<String>, facts: &mut Facts) {
    let Ok(lock) = ExtendedLockFile::read(&dir.join("flake.lock")) else {
        return;
    };
    for (_, node) in lock.root_input_nodes() {
        let Some(locked) = lock.nodes.get(&node).and_then(|n| n.locked.as_ref()) else {
            continue;
        };
        if locked.kind != "github" {
            continue;
        }
        let (Some(owner), Some(dep), Some(rev)) = (&locked.owner, &locked.repo, &locked.rev)
        else {
            continue;
        };
        if owner.eq_ignore_ascii_case(org) && dep != repo && known.contains(dep) {
            facts
                .pins
                .insert((repo.to_owned(), format!("flake:{dep}"), rev.clone()));
        }
    }
}

fn cargo_pins(dir: &Path, repo: &str, org: &str, known: &BTreeSet<String>, facts: &mut Facts) {
    let Ok(raw) = std::fs::read_to_string(dir.join("Cargo.lock")) else {
        return;
    };
    let Ok(doc) = raw.parse::<toml::Table>() else {
        return;
    };
    let Some(packages) = doc.get("package").and_then(toml::Value::as_array) else {
        return;
    };
    let prefix = format!("git+https://github.com/{}/", org.to_ascii_lowercase());
    for p in packages {
        let (Some(name), Some(source)) = (
            p.get("name").and_then(toml::Value::as_str),
            p.get("source").and_then(toml::Value::as_str),
        ) else {
            continue;
        };
        let lowered = source.to_ascii_lowercase();
        let Some(rest) = lowered.strip_prefix(&prefix) else {
            continue;
        };
        let Some((_, sha)) = source.rsplit_once('#') else {
            continue;
        };
        let origin = rest
            .split(['.', '?', '#', '/'])
            .next()
            .unwrap_or_default();
        if origin.is_empty() || origin == repo || !known.contains(origin) {
            continue;
        }
        facts.edges.insert((repo.to_owned(), origin.to_owned()));
        facts.pins.insert((
            repo.to_owned(),
            format!("crate:{name}"),
            sha.chars().take(12).collect(),
        ));
    }
}

pub(crate) fn render(proposal: &Proposal) -> String {
    match proposal {
        Proposal::Ok {
            waves,
            skews,
            moves,
            blocked,
        } => {
            let mut out = String::new();
            if !blocked.is_empty() {
                out.push_str(&format!(
                    "blocked: {} repo(s) sit on a dependency cycle or depend on one and have no order; break the cycle first: {}\n",
                    blocked.len(),
                    blocked.join(", ")
                ));
            }
            if skews.is_empty() {
                out.push_str(
                    "aligned: no repo in an orderable dependency tree pins a different version of a shared input\n",
                );
                return out;
            }
            let by_wave: BTreeMap<usize, Vec<&Move>> =
                moves.iter().fold(BTreeMap::new(), |mut acc, m| {
                    acc.entry(m.wave).or_default().push(m);
                    acc
                });
            out.push_str(&format!(
                "{} skewed input(s) across {} wave(s); propose, dependency first:\n",
                skews.len(),
                waves.len()
            ));
            for (wave, ms) in by_wave {
                out.push_str(&format!("  wave {wave}\n"));
                for m in ms {
                    out.push_str(&format!(
                        "    {}: {} {} -> {}\n",
                        m.repo, m.item, m.from, m.to
                    ));
                }
            }
            out
        }
        Proposal::Refused { why } => format!("refused: the order failed its own check ({why})\n"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(edges: &[(&str, &str)], pins: &[(&str, &str, &str)]) -> Facts {
        Facts {
            edges: edges
                .iter()
                .map(|(a, b)| ((*a).to_owned(), (*b).to_owned()))
                .collect(),
            pins: pins
                .iter()
                .map(|(r, i, v)| ((*r).to_owned(), (*i).to_owned(), (*v).to_owned()))
                .collect(),
        }
    }

    fn acyclic(n: usize, edges: &[(usize, usize)]) -> bool {
        let mut left: Vec<usize> = (0..n).collect();
        loop {
            let ready: Vec<usize> = left
                .iter()
                .copied()
                .filter(|v| {
                    edges
                        .iter()
                        .all(|(r, d)| r != v || !left.contains(d))
                })
                .collect();
            if ready.is_empty() {
                return left.is_empty();
            }
            left.retain(|v| !ready.contains(v));
        }
    }

    #[test]
    fn every_small_graph_is_ordered_dependency_first_or_named_as_a_cycle() {
        let mut engine = Engine::new().unwrap();
        for n in 2..=4usize {
            let pairs: Vec<(usize, usize)> = (0..n)
                .flat_map(|a| (0..n).filter(move |b| *b != a).map(move |b| (a, b)))
                .collect();
            for mask in 1u32..(1 << pairs.len()) {
                let chosen: Vec<(usize, usize)> = pairs
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| mask & (1 << i) != 0)
                    .map(|(_, p)| *p)
                    .collect();
                let names: Vec<(String, String)> = chosen
                    .iter()
                    .map(|(a, b)| (format!("r{a}"), format!("r{b}")))
                    .collect();
                let f = Facts {
                    edges: names.iter().cloned().collect(),
                    pins: BTreeSet::new(),
                };
                let touched: BTreeSet<&String> =
                    names.iter().flat_map(|(a, b)| [a, b]).collect();
                let verdict = engine.propose(&f).unwrap();
                let is_dag = acyclic(n, &chosen);
                let Proposal::Ok { waves, blocked, .. } = verdict else {
                    panic!("refused {names:?}");
                };
                assert_eq!(blocked.is_empty(), is_dag, "cycle verdict wrong: {names:?}");
                let stuck: BTreeSet<&String> = blocked.iter().collect();
                let placed: Vec<&String> = waves.iter().flatten().collect();
                assert!(
                    placed.iter().all(|r| !stuck.contains(*r)),
                    "a cyclic tree was ordered: {names:?} {waves:?}"
                );
                assert_eq!(
                    placed.len() + stuck.len(),
                    touched.len(),
                    "every node is placed or named stuck: {names:?}"
                );
                let at = |r: &String| waves.iter().position(|w| w.contains(r));
                for (r, d) in &names {
                    if stuck.contains(r) {
                        continue;
                    }
                    assert!(
                        at(d).unwrap() < at(r).unwrap(),
                        "{d} must precede {r} in {waves:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn the_blue_order_agrees_with_the_rust_update_chain() {
        let deps: std::collections::HashMap<String, Vec<String>> = [
            ("app", vec!["lib", "base"]),
            ("lib", vec!["base"]),
            ("tool", vec!["base"]),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_owned(), v.into_iter().map(str::to_owned).collect()))
        .collect();
        let rust: Vec<String> = crate::flake::compute_update_chain_all(&deps)
            .unwrap()
            .into_iter()
            .map(|s| s.repo)
            .collect();
        let f = Facts {
            edges: deps
                .iter()
                .flat_map(|(r, ds)| ds.iter().map(move |d| (r.clone(), d.clone())))
                .collect(),
            pins: BTreeSet::new(),
        };
        let Proposal::Ok { waves, .. } = propose(&f).unwrap() else {
            panic!("a DAG must be ordered");
        };
        let blue: Vec<String> = waves.into_iter().flatten().collect();
        let mut a = rust.clone();
        let mut b = blue.clone();
        a.sort();
        b.sort();
        b.retain(|r| r != "base");
        assert_eq!(a, b, "same repos ordered, the Rust chain omits the root");
        let pos = |v: &Vec<String>, r: &str| v.iter().position(|x| x == r).unwrap();
        for (r, ds) in &deps {
            for d in ds {
                assert!(pos(&blue, d) < pos(&blue, r));
                if rust.contains(d) {
                    assert!(pos(&rust, d) < pos(&rust, r));
                }
            }
        }
    }

    #[test]
    fn a_skewed_pin_is_proposed_not_applied() {
        let f = facts(
            &[("app", "lib"), ("lib", "base")],
            &[
                ("lib", "flake:base", "r1"),
                ("app", "flake:base", "r2"),
                ("app", "crate:kit", "k1"),
                ("lib", "crate:kit", "k1"),
            ],
        );
        let Proposal::Ok { skews, moves, .. } = propose(&f).unwrap() else {
            panic!("ordered");
        };
        assert_eq!(skews.len(), 1);
        assert_eq!(skews[0].item, "flake:base");
        assert_eq!(skews[0].target, "r2");
        assert_eq!(
            moves,
            vec![Move {
                wave: 1,
                repo: "lib".into(),
                item: "flake:base".into(),
                from: "r1".into(),
                to: "r2".into()
            }]
        );
    }

    #[test]
    fn a_cycle_is_named_and_the_other_trees_still_get_a_plan() {
        let f = facts(
            &[("a", "b"), ("b", "a"), ("app", "lib")],
            &[("app", "i", "x"), ("lib", "i", "y")],
        );
        let Proposal::Ok {
            blocked,
            moves,
            waves,
            ..
        } = propose(&f).unwrap()
        else {
            panic!("ordered");
        };
        assert_eq!(blocked, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(waves, vec![vec!["lib".to_string()], vec!["app".to_string()]]);
        assert_eq!(moves.len(), 1);
    }

    #[test]
    fn gather_reads_flake_and_cargo_pins_of_the_org() {
        let dir = tempfile::tempdir().unwrap();
        for r in ["base", "lib", "app"] {
            std::fs::create_dir_all(dir.path().join(r).join(".git")).unwrap();
        }
        std::fs::write(
            dir.path().join("app").join("Cargo.lock"),
            "[[package]]\nname = \"kit\"\nversion = \"0.1.0\"\nsource = \"git+https://github.com/pleme-io/base?rev=x#0123456789abcdef\"\n\n[[package]]\nname = \"serde\"\nversion = \"1.0.0\"\nsource = \"registry+https://github.com/rust-lang/crates.io-index\"\n",
        )
        .unwrap();
        let facts = gather_at(dir.path(), "pleme-io", &FlakeGraph::default()).unwrap();
        assert!(facts.edges.contains(&("app".into(), "base".into())));
        assert!(facts
            .pins
            .contains(&("app".into(), "crate:kit".into(), "0123456789ab".into())));
        assert_eq!(facts.pins.len(), 1);
    }
}
