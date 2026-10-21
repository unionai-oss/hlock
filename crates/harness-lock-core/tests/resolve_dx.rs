//! `resolve`'s checks before a search, the holdout check, budgets, the trial cache, and
//! the report (specs/resolve-tune-dx.md).

use std::fs;
use std::path::{Path, PathBuf};

use harness_lock_core::{Manifest, Resolver};

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hlock-resolve-dx-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// A pack of three tasks, so the 70/30 split leaves one for the holdout.
fn pack(root: &Path) -> PathBuf {
    let dir = root.join("evals");
    let mut toml = String::from("[pack]\nname = \"t/dx\"\nversion = \"0.1.0\"\ndomain = \"demo\"\n");
    for id in ["a", "b", "c"] {
        fs::create_dir_all(dir.join(id)).unwrap();
        fs::write(dir.join(id).join("prompt.md"), format!("Write {id}.txt\n")).unwrap();
        toml.push_str(&format!("\n[[task]]\nid = \"{id}\"\nprompt = \"{id}/prompt.md\"\nexpected = {{ artifacts = [\"{id}.txt\"] }}\n"));
    }
    fs::write(dir.join("pack.toml"), toml).unwrap();
    dir
}

fn manifest(root: &Path, extra: &str) -> Manifest {
    let text = format!(
        r#"
[project]
name = "dx"

[evals]
packs = ["{}"]
seeds = 2

[candidates.llm.planner]
any_of = ["reference:qwen-7b", "reference:qwen-32b"]

[candidates]
decision = ["none"]
harness = ["reference"]

[objective]
primary = "success-rate"
select = "min-cost"
{extra}
"#,
        pack(root).display()
    );
    Manifest::from_str(&text).unwrap()
}

#[test]
fn budget_keys_load_in_either_spelling() {
    let root = tmp("budget");
    let m = manifest(&root, "[budget]\nmax_trials = 5\nmax-spend-usd = 2.5\nrandom_seed = 7\n");
    assert_eq!((m.budget.max_trials, m.budget.max_spend_usd, m.budget.random_seed), (Some(5), Some(2.5), 7));
    assert_eq!(m.budget.strategy, "grid");
    assert!(m.problems().is_empty(), "{:?}", m.problems());
}

#[test]
fn unknown_keys_and_unsupported_rules_stop_a_search() {
    let root = tmp("problems");
    let m = manifest(&root, "[budget]\nmax_trial = 5\nstrategy = \"bayes\"\n\n[evals.splits]\nserch = 0.5\n");
    let problems = m.problems();
    for want in [
        "unknown key `max_trial` in [budget] (did you mean `max-trials`?)",
        "unknown key `serch` in [evals.splits] (did you mean `search`?)",
        "[budget] strategy = \"bayes\" isn't supported (use \"grid\")",
    ] {
        assert!(problems.iter().any(|p| p == want), "{want:?} not in {problems:?}");
    }
    let err = Resolver::new(m, root.join(".hlock")).resolve_lock().unwrap_err().to_string();
    assert!(err.contains("max_trial"), "{err}");
    let mut m = manifest(&root, "");
    m.objective.select = "max-qualty".into();
    let err = Resolver::new(m, root.join(".hlock")).resolve_lock().unwrap_err().to_string();
    assert!(err.contains("select = \"max-qualty\" isn't supported"), "{err}");
}

#[test]
fn the_winner_is_checked_on_the_holdout() {
    let root = tmp("holdout");
    let mut r = Resolver::new(manifest(&root, ""), root.join(".hlock"));
    let lock = r.resolve_lock().unwrap();
    let ev = &lock.evidence;
    assert_eq!(ev.split, "holdout");
    assert_eq!(ev.trials, 2, "1 holdout task × 2 seeds");
    assert!(ev.success_rate.contains_key("search") && ev.success_rate.contains_key("mean"));
    // provenance counts stacks and trials apart
    assert_eq!(lock.provenance.search["candidates"], 2);
    assert_eq!(lock.provenance.search["trials"], 8, "2 stacks × 2 tasks × 2 seeds");
    let report = r.report.clone().unwrap();
    assert_eq!(report["stacks"].as_array().unwrap().len(), 2);
    assert_eq!(report["stacks"].as_array().unwrap().iter().filter(|s| s["winner"] == true).count(), 1);
    assert_eq!(report["holdout"]["trials"], 2);
    let md = fs::read_to_string(root.join(".hlock/report.md")).unwrap();
    assert!(md.contains("| stack |") && md.contains("★") && md.contains("Holdout:"), "{md}");
    // workspace names are hex digits, never `sha256:`
    for e in fs::read_dir(root.join(".hlock/runs")).unwrap().flatten() {
        assert!(!e.file_name().to_string_lossy().contains(':'), "{:?}", e.file_name());
    }
}

#[test]
fn no_holdout_tasks_means_in_sample_evidence_and_a_note() {
    let root = tmp("insample");
    let mut m = manifest(&root, "");
    m.evals.insert("splits".into(), serde_json::json!({"search": 1.0}));
    let notes = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink = notes.clone();
    let r = Resolver::new(m, root.join(".hlock"));
    *r.on_event.lock().unwrap() = Some(Box::new(move |e: serde_json::Value| {
        if let Some(t) = e["text"].as_str() {
            sink.lock().unwrap().push(t.to_string());
        }
    }));
    let mut r = r;
    let lock = r.resolve_lock().unwrap();
    assert_eq!(lock.evidence.split, "search");
    assert!(!lock.evidence.success_rate.contains_key("search"));
    assert!(notes.lock().unwrap().iter().any(|n| n.contains("in-sample") && n.contains("no holdout tasks")));
}

#[test]
fn trials_come_from_the_cache_unless_told_not_to() {
    let root = tmp("cache");
    let m = manifest(&root, "");
    let mut r = Resolver::new(m.clone(), root.join(".hlock"));
    r.resolve_lock().unwrap();
    assert_eq!(r.report.unwrap()["cached"], 0);
    let mut r = Resolver::new(m.clone(), root.join(".hlock"));
    let lock = r.resolve_lock().unwrap();
    assert_eq!(r.report.unwrap()["cached"], 10, "8 search + 2 holdout");
    assert_eq!(lock.provenance.search["cached"], 10);
    let mut r = Resolver::new(m.clone(), root.join(".hlock"));
    r.use_cache = false;
    r.resolve_lock().unwrap();
    assert_eq!(r.report.unwrap()["cached"], 0);
    // a changed task is a different pack: nothing to reuse
    fs::write(root.join("evals/a/prompt.md"), "Write a.txt, carefully\n").unwrap();
    let mut r = Resolver::new(m, root.join(".hlock"));
    r.resolve_lock().unwrap();
    assert_eq!(r.report.unwrap()["cached"], 0);
}

#[test]
fn max_spend_stops_trials_before_the_cap() {
    let root = tmp("spend");
    let m = manifest(&root, "[budget]\nmax-spend-usd = 0.0005\n");
    let mut r = Resolver::new(m, root.join(".hlock"));
    r.use_cache = false;
    let lock = r.resolve_lock().unwrap();
    let report = r.report.unwrap();
    assert!(report["spend-usd"].as_f64().unwrap() <= 0.0005 + 1e-9, "{report}");
    assert!(lock.provenance.search["trials"].as_u64().unwrap() < 8);
}

#[test]
fn an_agent_cli_with_reference_models_is_refused_before_any_trial() {
    let root = tmp("cli");
    let m = manifest(&root, "[target]\nharness = \"opencode\"\n");
    let err = Resolver::new(m, root.join(".hlock")).resolve_lock().unwrap_err().to_string();
    assert!(err.contains("opencode can't run reference:qwen-7b"), "{err}");
    assert!(!root.join(".hlock/runs").exists(), "no trial ran");
}

#[test]
fn too_many_stacks_is_an_error_not_a_cut() {
    let root = tmp("stacks");
    let mut m = manifest(&root, "");
    let models: Vec<String> = (0..201).map(|i| format!("reference:m{i}")).collect();
    m.candidates.llm.get_mut("planner").unwrap().any_of = models;
    let err = Resolver::new(m, root.join(".hlock")).resolve_lock().unwrap_err().to_string();
    assert!(err.contains("201 stacks") && err.contains("narrow [candidates]"), "{err}");
}

#[test]
fn plan_counts_without_running() {
    let root = tmp("plan");
    let mut r = Resolver::new(manifest(&root, "[budget]\nmax-trials = 5\n"), root.join(".hlock"));
    let plan = r.plan().unwrap();
    assert_eq!(plan["stacks"].as_array().unwrap().len(), 2);
    assert_eq!((plan["all-trials"].as_u64(), plan["trials"].as_u64(), plan["holdout-trials"].as_u64()), (Some(8), Some(5), Some(2)));
    assert!(!root.join(".hlock/runs").exists());
}
