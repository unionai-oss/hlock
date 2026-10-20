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
target = "local"
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
