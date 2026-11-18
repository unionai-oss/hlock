//! End-to-end tests for `hlock upgrade` (§5.8) and `hlock tune` (§5.9).
//!
//! These chdir, so they are consolidated into a single test that runs
//! sequentially within the process (integration-test threads share the cwd).

use std::fs;
use std::path::Path;

use harness_lock_core::ops::{plan_upgrade, tune, upgrade, TuneSpec, UpgradeSpec};
use harness_lock_core::{Lockfile, Manifest, Resolver, LOCKFILE_PATH};

fn make_pack(root: &Path) {
    let taskdir = root.join("t1");
    fs::create_dir_all(&taskdir).unwrap();
    fs::write(taskdir.join("prompt.md"), "Produce an executive summary as summary.md").unwrap();
    fs::write(
        root.join("pack.toml"),
        r#"
[pack]
name = "test/pack"
version = "0.1.0"
license = "Apache-2.0"
domain = "demo"

[[task]]
id = "summarize"
prompt = "t1/prompt.md"
expected = { artifacts = ["summary.md"] }
"#,
    )
    .unwrap();
}

fn make_manifest(pack: &Path) -> Manifest {
    let toml = r#"
[project]
name = "opstest"
[evals]
seeds = 2
[candidates.llm.planner]
any_of = ["reference:qwen-7b", "reference:qwen-32b"]
[candidates]
decision = ["none"]
harness = ["reference"]
[objective]
primary = "success-rate"
select = "max-quality"
[objective.constraints]
success-rate = ">=0.5"
[budget]
strategy = "grid"
[backend]
target = "offline"
"#;
    let mut m = Manifest::from_str(toml).unwrap();
    m.evals.insert("packs".to_string(), serde_json::json!([pack.to_string_lossy().to_string()]));
    m
}

#[test]
fn ops_end_to_end_upgrade_and_tune() {
    let dir = std::env::temp_dir().join(format!("hlock-ops-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    let cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(&dir).unwrap();

    make_pack(&dir);
    let manifest = make_manifest(&dir);
    let _ = Resolver::new(manifest.clone(), dir.join(".hlock")).resolve().unwrap();
    let base = Lockfile::load(Path::new(LOCKFILE_PATH)).unwrap();
    let base_model = base.llm[0].model.clone();

    // ---- upgrade: plan, apply, rollback ----
    let plan = plan_upgrade(Some(&base), &manifest, &UpgradeSpec::default());
    assert!(!plan["upgradeable"].as_array().unwrap().is_empty());

    let apply_spec = UpgradeSpec { all: true, apply: true, workdir: dir.join(".hlock"), ..Default::default() };
    let applied = upgrade(manifest.clone(), &apply_spec).unwrap();
    assert!(applied.applied);
    assert!(dir.join(format!("{LOCKFILE_PATH}.prev")).exists());
    assert_eq!(Lockfile::load(Path::new(LOCKFILE_PATH)).unwrap().llm[0].model, "qwen-32b");

    let rb = UpgradeSpec { rollback: true, apply: false, workdir: dir.join(".hlock"), ..Default::default() };
    assert!(upgrade(manifest.clone(), &rb).unwrap().rolled_back);
    assert_eq!(Lockfile::load(Path::new(LOCKFILE_PATH)).unwrap().llm[0].model, base_model);

    // ---- tune ----
    let tune_spec = TuneSpec {
        depth: "hpo".into(),
        data: "user".into(),
        out: dir.join("harness.tune.lock"),
        workdir: dir.join(".hlock"),
        ..Default::default()
    };
    let outcome = tune(manifest.clone(), &tune_spec).unwrap();
    assert!(outcome.path.exists());
    let tuned = Lockfile::load(&outcome.path).unwrap();
    assert_eq!(tuned.kind.as_deref(), Some("tuned"));
    assert!(tuned.base_lock.is_some());
    assert!(tuned.tune.is_some());
    // the opt grid is models x retries and must have run >= 1 config
    assert!(outcome.report["grid"].as_u64().unwrap() >= 1);

    std::env::set_current_dir(&cwd).unwrap();
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_lock_that_doesnt_load_is_an_error_not_a_zero_baseline() {
    let dir = std::env::temp_dir().join(format!("hlock-ops-corrupt-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    make_pack(&dir);
    let lock = dir.join("harness.lock");
    fs::write(&lock, "this isn't = [ a lock").unwrap();
    let tune_spec = TuneSpec { strict: true, out: dir.join("harness.tune.lock"), workdir: dir.join(".hlock"), lock: Some(lock.clone()), ..Default::default() };
    assert!(tune(make_manifest(&dir), &tune_spec).is_err());
    assert!(!dir.join("harness.tune.lock").exists());
    let up = UpgradeSpec { all: true, apply: true, workdir: dir.join(".hlock"), lock: Some(lock.clone()), ..Default::default() };
    assert!(upgrade(make_manifest(&dir), &up).is_err());
    assert_eq!(fs::read_to_string(&lock).unwrap(), "this isn't = [ a lock", "left alone");
    // with no lock, the baseline is the manifest's first configuration, measured
    fs::remove_file(&lock).unwrap();
    let o = tune(make_manifest(&dir), &TuneSpec { lock: Some(lock), strict: false, ..tune_spec }).unwrap();
    assert_eq!(o.report["baseline"]["measured"], true);
    assert!(o.report["baseline"]["trials"].as_u64().unwrap() > 0);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn an_upgrade_writes_each_roles_own_target() {
    let dir = std::env::temp_dir().join(format!("hlock-ops-roles-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    make_pack(&dir);
    let mut m = make_manifest(&dir);
    m.objective.constraints.clear();
    m.candidates.llm.get_mut("planner").unwrap().any_of = vec!["reference:qwen-7b".into()];
    m.candidates.llm.insert("coder".into(), harness_lock_core::manifest::LLMCandidate { any_of: vec!["reference:qwen-1.5b".into()], ..Default::default() });
    let lock = dir.join("harness.lock");
    Resolver::new(m.clone(), dir.join(".hlock")).resolve_lock().unwrap().save(&lock).unwrap();
    let up = UpgradeSpec { apply: true, force: true, workdir: dir.join(".hlock"), lock: Some(lock.clone()), ..Default::default() };
    let o = upgrade(m, &up).unwrap();
    assert!(o.applied, "{}", o.message);
    let roles: Vec<(String, String)> = Lockfile::load(&lock).unwrap().llm.iter().map(|l| (l.role.clone(), l.model.clone())).collect();
    assert_eq!(roles, [("coder".to_string(), "qwen-7b".to_string()), ("planner".into(), "qwen-32b".into())]);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn manifest_carries_candidate_models() {
    let m = Manifest::from_str(
        r#"
[project]
name = "g"
[candidates.llm.planner]
any_of = ["reference:qwen-7b", "reference:qwen-32b"]
[candidates]
harness = ["reference"]
"#,
    )
    .unwrap();
    assert_eq!(m.candidates.llm.get("planner").unwrap().any_of.len(), 2);
    let spec = TuneSpec { depth: "auto".into(), ..Default::default() };
    assert_eq!(spec.resolved_depth(), "hpo");
}

/// A command backend that runs only trials: each writes the task's artifact and passes.
#[cfg(unix)]
fn trials_only_backend(dir: &Path) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let script = dir.join("trials-only");
    fs::write(
        &script,
        r#"#!/bin/sh
if grep -q '"kind":"backend.describe"' "$1"; then
  echo '{"version": 4, "ok": true, "result": {"name": "trials-only", "kinds": ["llm.run_trial"], "about": "test"}}' > "$2"
else
  echo done > "$(dirname "$1")/summary.md"
  echo '{"version": 4, "ok": true, "result": {"success": true, "cost_usd": 0.001, "steps": 1}}' > "$2"
fi
"#,
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    script
}

#[cfg(unix)]
#[test]
fn a_custom_backend_runs_what_it_says_it_runs() {
    let dir = std::env::temp_dir().join(format!("hlock-ops-custom-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    make_pack(&dir);
    let script = trials_only_backend(&dir);
    let mut m = make_manifest(&dir);
    m.backends.insert("mine".into(), serde_json::json!({"command": [script.to_string_lossy()], "options": {"k": "v"}}));
    let spec = TuneSpec {
        depth: "prompt".into(),
        backend: Some("mine".into()),
        out: dir.join("harness.tune.lock"),
        workdir: dir.join(".hlock"),
        lock: Some(dir.join("harness.lock")),
        ..Default::default()
    };
    let o = tune(m.clone(), &spec).unwrap();
    assert_eq!(o.report["backend"], "mine");
    let stages = o.report["stages"].as_array().unwrap();
    let detail = |name: &str| stages.iter().find(|s| s["stage"] == name).map(|s| (s["ran"].clone(), s["detail"].clone())).unwrap();
    assert_eq!(detail("data"), (serde_json::json!(false), serde_json::json!("the mine backend doesn't run data.load")));
    // it doesn't host the search, so hlock's local worker does, when this machine has one
    let (ran, why) = detail("optimize");
    assert!(ran == true || why.as_str().unwrap().starts_with("the mine backend doesn't host the search, and hlock's local worker can't"), "{why}");
    // the trials ran on it, for real: the backend's cost, not the reference's
    assert_eq!(o.report["best"]["success_rate"], 1.0);
    assert_eq!(o.report["best"]["cost_usd"], 0.001);
    assert_eq!(o.lock.provenance.backend, "mine");

    // a name nothing defines is asked of the Python worker (its entry points), and fails
    // before any trial runs (what it says is tested on the Python side)
    let out = dir.join("nowhere.tune.lock");
    assert!(tune(m, &TuneSpec { backend: Some("nowhere".into()), out: out.clone(), ..spec }).is_err());
    assert!(!out.exists());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_rust_backend_runs_in_process() {
    use harness_lock_core::worker::{Describe, Executor, TaskRequest, TaskResult};
    struct Always;
    impl Executor for Always {
        fn describe(&self, _: &TaskRequest) -> harness_lock_core::Result<Describe> {
            Ok(Describe::new("always", ["llm.run_trial"]))
        }
        fn call(&self, req: &TaskRequest) -> harness_lock_core::Result<TaskResult> {
            fs::write(Path::new(&req.workdir).join("summary.md"), "done")?;
            Ok(TaskResult { success: true, cost_usd: Some(0.5), steps: 1, ..Default::default() })
        }
    }
    let dir = std::env::temp_dir().join(format!("hlock-ops-embedded-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    make_pack(&dir);
    let mut r = Resolver::with_executor(make_manifest(&dir), dir.join(".hlock"), "always", Box::new(Always));
    let lock = r.resolve_lock().unwrap();
    assert_eq!(lock.provenance.backend, "always");
    assert_eq!(lock.evidence.success_rate.values().next(), Some(&1.0));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_costlier_upgrade_is_rolled_back_by_the_gate() {
    let dir = std::env::temp_dir().join(format!("hlock-ops-gate-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    make_pack(&dir);
    let mut m = make_manifest(&dir);
    m.candidates.llm.get_mut("planner").unwrap().any_of = vec!["reference:qwen-7b".into()];
    let lock = dir.join("harness.lock");
    let base = Resolver::new(m.clone(), dir.join(".hlock")).resolve_lock().unwrap();
    // the reference harness's trace has each call's tokens
    assert!(base.evidence.behavior.as_ref().unwrap().output_tokens_per_task.unwrap() > 0.0);
    base.save(&lock).unwrap();
    let up = UpgradeSpec { apply: true, workdir: dir.join(".hlock"), lock: Some(lock.clone()), ..Default::default() };

    // qwen-32b holds the success rate, at about 4x the cost per call
    m.gate.insert("max-cost-increase".into(), serde_json::json!(0.5));
    let o = upgrade(m.clone(), &up).unwrap();
    assert!(!o.applied, "{}", o.message);
    assert!(o.message.contains("upgrade held success-rate") && o.message.contains("max-cost-increase allows") && o.message.contains("rolled back"), "{}", o.message);
    assert_eq!(Lockfile::load(&lock).unwrap().llm[0].model, "qwen-7b");
    assert!(o.delta["cost-usd-per-task"]["change"].as_f64().unwrap() > 0.5);

    // --force takes it anyway; a looser gate lets it through, saying what it costs
    assert!(upgrade(m.clone(), &UpgradeSpec { force: true, ..up.clone() }).unwrap().applied);
    base.save(&lock).unwrap();
    m.gate.insert("max-cost-increase".into(), serde_json::json!(10.0));
    let o = upgrade(m.clone(), &up).unwrap();
    assert!(o.applied && o.message.contains("cost/task $") && o.message.contains("output tokens/task"), "{}", o.message);
    assert_eq!(Lockfile::load(&lock).unwrap().llm[0].model, "qwen-32b");

    // a gate that doesn't parse stops the apply before anything runs
    base.save(&lock).unwrap();
    m.gate.insert("max-cost-increase".into(), serde_json::json!("lots"));
    assert!(upgrade(m, &up).unwrap_err().to_string().contains("[gate] max-cost-increase"));
    assert_eq!(Lockfile::load(&lock).unwrap().llm[0].model, "qwen-7b");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn tune_doesnt_pick_a_configuration_the_gate_rules_out() {
    let dir = std::env::temp_dir().join(format!("hlock-ops-tune-gate-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    make_pack(&dir);
    let mut m = make_manifest(&dir);
    m.gate.insert("max-cost-increase".into(), serde_json::json!(0.0));
    let spec = TuneSpec { depth: "probe".into(), out: dir.join("harness.tune.lock"), workdir: dir.join(".hlock"), lock: Some(dir.join("harness.lock")), ..Default::default() };
    let o = tune(m, &spec).unwrap();
    // qwen-32b's configurations cost more than the base (qwen-7b)
    let notes: Vec<&str> = o.report["notes"].as_array().unwrap().iter().filter_map(|n| n.as_str()).collect();
    assert!(notes.iter().any(|n| n.contains("cost more than the cost limits allow")), "{notes:?}");
    assert_eq!(o.report["best"]["model"], "reference:qwen-7b");
    assert!(o.report["spend"]["cost-usd-per-task"]["change"].as_f64().unwrap() <= 0.0, "{}", o.report["spend"]);
    assert!(o.report["behavior"]["best"]["output-tokens-per-task"].as_f64().is_some());
    assert!(o.lock.evidence.behavior.is_some());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_model_that_rejects_temperature_isnt_swept_and_pins_none() {
    let dir = std::env::temp_dir().join(format!("hlock-ops-rejects-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    make_pack(&dir);
    let mut m = make_manifest(&dir);
    m.objective.constraints.clear();
    // Claude Fable 5.2 rejects temperature (profiles/models/anthropic.toml); qwen doesn't
    m.candidates.llm.get_mut("planner").unwrap().any_of = vec!["reference:claude-fable-5-2".into(), "reference:qwen-7b".into()];
    let spec = TuneSpec { depth: "hpo".into(), out: dir.join("first.lock"), workdir: dir.join(".hlock"), lock: Some(dir.join("none.lock")), ..Default::default() };
    let o = tune(m.clone(), &spec).unwrap();
    // 3 retries x (fable without a temperature + qwen at 2 temperatures)
    assert_eq!(o.report["grid"], 9, "{}", o.report);
    let notes: Vec<&str> = o.report["notes"].as_array().unwrap().iter().filter_map(|n| n.as_str()).collect();
    assert!(notes.iter().any(|n| n.starts_with("temperature not tuned: reference:claude-fable-5-2 rejects it")), "{notes:?}");
    assert!(!notes.iter().any(|n| n.contains("qwen")), "{notes:?}");
    let hpo = &o.lock.tune.as_ref().unwrap()["hpo"];
    assert_eq!(hpo["skipped"]["temperature"], "reference:claude-fable-5-2 rejects it");
    assert_eq!(hpo["axes"]["temperature"], serde_json::json!([0.2, 0.7]));

    // a fable-only tune from a lock that pinned a temperature drops it
    let mut base = o.lock.clone();
    let planner = base.llm.iter_mut().find(|l| l.role == "planner").unwrap();
    planner.provider = "reference".into();
    planner.model = "claude-fable-5-2".into();
    planner.params.insert("temperature".into(), serde_json::json!(0.7));
    base.save(&dir.join("base.lock")).unwrap();
    m.candidates.llm.get_mut("planner").unwrap().any_of = vec!["reference:claude-fable-5-2".into()];
    let o = tune(m, &TuneSpec { out: dir.join("second.lock"), lock: Some(dir.join("base.lock")), ..spec }).unwrap();
    assert_eq!(o.report["grid"], 3, "{}", o.report);
    assert!(o.report["best"]["temperature"].is_null(), "{}", o.report);
    assert_eq!(o.lock.tune.as_ref().unwrap()["hpo"]["axes"]["temperature"], serde_json::json!([]));
    let planner = o.lock.llm.iter().find(|l| l.role == "planner").unwrap();
    assert_eq!(planner.model, "claude-fable-5-2");
    assert!(!planner.params.contains_key("temperature"), "{:?}", planner.params);
    assert!(!Lockfile::load(&dir.join("second.lock")).unwrap().llm[0].params.contains_key("temperature"));
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_trial_out_of_tokens_is_counted_apart_and_not_cached() {
    use harness_lock_core::worker::{Describe, Executor, TaskRequest, TaskResult};
    // qwen-32b thinks its whole budget away and writes nothing; qwen-7b answers
    struct Thinker;
    impl Executor for Thinker {
        fn describe(&self, _: &TaskRequest) -> harness_lock_core::Result<Describe> {
            Ok(Describe::new("thinker", ["llm.run_trial"]))
        }
        fn call(&self, req: &TaskRequest) -> harness_lock_core::Result<TaskResult> {
            let thinks = req.stack["llm"]["planner"].as_str().is_some_and(|m| m.ends_with("32b"));
            if !thinks {
                fs::write(Path::new(&req.workdir).join("summary.md"), "done")?;
            }
            Ok(TaskResult { success: true, out_of_tokens: thinks, cost_usd: Some(0.0), steps: 1, ..Default::default() })
        }
    }
    let dir = std::env::temp_dir().join(format!("hlock-ops-out-of-tokens-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    // the pack apart from the workdir, so its digest (in the cache key) holds still
    make_pack(&dir.join("pack"));
    let resolve = || {
        let mut r = Resolver::with_executor(make_manifest(&dir.join("pack")), dir.join(".hlock"), "thinker", Box::new(Thinker));
        let notes = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = notes.clone();
        *r.on_event.lock().unwrap() = Some(Box::new(move |e: serde_json::Value| {
            if e["kind"] == "note" {
                sink.lock().unwrap().push(e["text"].as_str().unwrap_or_default().to_string());
            }
        }));
        let lock = r.resolve_lock().unwrap();
        let notes = notes.lock().unwrap().clone();
        (lock, r.report.clone().unwrap(), notes)
    };
    let (lock, report, notes) = resolve();
    assert_eq!(lock.llm[0].model, "qwen-7b");
    // the winner never ran out, so its evidence says nothing about it
    assert_eq!(lock.evidence.out_of_tokens, 0);
    assert_eq!(report["out-of-tokens"], 2);
    let big = report["stacks"].as_array().unwrap().iter().find(|s| s["label"].as_str().unwrap().contains("32b")).unwrap();
    assert_eq!((big["out-of-tokens"].as_u64(), big["success-rate"].as_f64()), (Some(2), Some(0.0)));
    assert!(notes.contains(&"2 trials ran out of tokens before answering; raise HL_LLM_MAX_TOKENS or lower effort".to_string()), "{notes:?}");
    // the trials that ran out run again next time (a raised budget may answer); the rest are cached
    let (_, report, _) = resolve();
    assert_eq!((report["cached"].as_u64(), report["out-of-tokens"].as_u64()), (Some(2), Some(2)));
}

#[test]
fn tune_sweeps_effort_when_the_manifest_lists_levels_and_keeps_the_locks_on_a_tie() {
    let dir = std::env::temp_dir().join(format!("hlock-ops-effort-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    make_pack(&dir);
    let mut m = make_manifest(&dir);
    let planner = m.candidates.llm.get_mut("planner").unwrap();
    planner.any_of = vec!["reference:qwen-32b".into()];
    planner.effort = vec!["medium".into(), "high".into(), "xhigh".into()];
    let lock = dir.join("harness.lock");
    let spec = TuneSpec { depth: "probe".into(), out: dir.join("harness.tune.lock"), workdir: dir.join(".hlock"), lock: Some(lock.clone()), ..Default::default() };

    // no lock: one model x three efforts x retries [0]; the first level is the base
    let o = tune(m.clone(), &spec).unwrap();
    assert_eq!(o.report["grid"], 3);
    assert_eq!(o.lock.tune.as_ref().unwrap()["hpo"]["axes"]["effort"], serde_json::json!(["medium", "high", "xhigh"]));
    assert_eq!(o.report["best"]["effort"], "medium");
    assert_eq!(o.lock.llm[0].effort.as_deref(), Some("medium"));
    assert_eq!(o.lock.tune.as_ref().unwrap()["hpo"]["best"]["effort"], "medium");

    // the reference ties every level, so tune keeps the lock's own effort
    let mut base = o.lock.clone();
    base.kind = None;
    base.llm[0].effort = Some("xhigh".into());
    base.save(&lock).unwrap();
    let o = tune(m.clone(), &spec).unwrap();
    assert_eq!(o.report["best"]["effort"], "xhigh");
    assert_eq!(Lockfile::load(&spec.out).unwrap().llm[0].effort.as_deref(), Some("xhigh"));

    // one level isn't an axis: the lock's effort stays
    m.candidates.llm.get_mut("planner").unwrap().effort = vec!["low".into()];
    let o = tune(m.clone(), &spec).unwrap();
    assert_eq!((o.report["grid"].as_u64(), o.report["best"]["effort"].as_str()), (Some(1), Some("xhigh")));
    // and with none anywhere, none is sent or recorded
    m.candidates.llm.get_mut("planner").unwrap().effort.clear();
    base.llm[0].effort = None;
    base.save(&lock).unwrap();
    let o = tune(m, &spec).unwrap();
    assert!(o.report["best"].get("effort").is_none() && o.lock.llm[0].effort.is_none());
    assert!(o.lock.tune.as_ref().unwrap()["hpo"]["axes"].get("effort").is_none());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_trace_with_tool_calls_gives_the_locks_behavior() {
    use harness_lock_core::worker::{Describe, Executor, TaskRequest, TaskResult};
    struct Chatty;
    impl Executor for Chatty {
        fn describe(&self, _: &TaskRequest) -> harness_lock_core::Result<Describe> {
            Ok(Describe::new("chatty", ["llm.run_trial"]))
        }
        fn call(&self, req: &TaskRequest) -> harness_lock_core::Result<TaskResult> {
            fs::write(Path::new(&req.workdir).join("summary.md"), "done")?;
            // Claude Code's stream-json: three tool calls in a row, a subagent, a last message offering more
            let trace = serde_json::json!([
                {"type": "assistant", "message": {"id": "a", "content": [{"type": "tool_use", "name": "Read"}, {"type": "tool_use", "name": "Task"}, {"type": "tool_use", "name": "Write"}], "usage": {"input_tokens": 100, "output_tokens": 40}}},
                {"type": "assistant", "message": {"id": "b", "content": [{"type": "text", "text": "Wrote it. Would you like me to continue?"}], "usage": {"input_tokens": 120, "output_tokens": 10}}},
            ]);
            Ok(TaskResult { success: true, cost_usd: None, steps: 2, trace, ..Default::default() })
        }
    }
    let dir = std::env::temp_dir().join(format!("hlock-ops-behavior-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    make_pack(&dir);
    let lock = Resolver::with_executor(make_manifest(&dir), dir.join(".hlock"), "chatty", Box::new(Chatty)).resolve_lock().unwrap();
    let b = lock.evidence.behavior.clone().unwrap();
    assert_eq!((b.input_tokens_per_task, b.output_tokens_per_task, b.tool_calls_per_task), (Some(220.0), Some(50.0), Some(3.0)));
    assert_eq!((b.longest_silent_stretch_max, b.subagents_per_task, b.approval_request_rate, b.early_stop_rate), (Some(3), Some(1.0), Some(0.0), Some(1.0)));
    let path = dir.join("harness.lock");
    lock.save(&path).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("[evidence.behavior]") && text.contains("early-stop-rate = 1.0"), "{text}");
    assert_eq!(Lockfile::load(&path).unwrap().evidence.behavior, lock.evidence.behavior);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn resolve_searches_effort_and_a_trial_gets_it() {
    use harness_lock_core::worker::{Describe, Executor, TaskRequest, TaskResult};
    // passes only at high effort, so the search has to find it
    struct HighOnly;
    impl Executor for HighOnly {
        fn describe(&self, _: &TaskRequest) -> harness_lock_core::Result<Describe> {
            Ok(Describe::new("high-only", ["llm.run_trial"]))
        }
        fn call(&self, req: &TaskRequest) -> harness_lock_core::Result<TaskResult> {
            let high = req.stack["effort"]["planner"] == "high";
            if high {
                fs::write(Path::new(&req.workdir).join("summary.md"), "done")?;
            }
            Ok(TaskResult { success: high, cost_usd: Some(0.1), steps: 1, ..Default::default() })
        }
    }
    let dir = std::env::temp_dir().join(format!("hlock-ops-effort-resolve-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    make_pack(&dir);
    let mut m = make_manifest(&dir);
    m.objective.constraints.clear();
    let planner = m.candidates.llm.get_mut("planner").unwrap();
    planner.any_of = vec!["reference:qwen-7b".into()];
    planner.effort = vec!["low".into(), "high".into()];
    let mut r = Resolver::with_executor(m, dir.join(".hlock"), "high-only", Box::new(HighOnly));
    let labels: Vec<String> = r.enumerate_stacks().unwrap().iter().map(|s| s.label()).collect();
    assert_eq!(labels, ["reference/reference:qwen-7b@low/d=none", "reference/reference:qwen-7b@high/d=none"]);
    let lock = r.resolve_lock().unwrap();
    assert_eq!(lock.llm[0].effort.as_deref(), Some("high"));
    let _ = fs::remove_dir_all(&dir);
}
