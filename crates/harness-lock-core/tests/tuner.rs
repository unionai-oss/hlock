//! End-to-end tuner test: build a temp task pack, run `Tuner::tune`, and check
//! the produced `harness.lock`.

use std::fs;
use std::path::{Path, PathBuf};

use harness_lock_core::{Manifest, Tuner};

fn make_pack(root: &Path) -> PathBuf {
    let taskdir = root.join("t1");
    fs::create_dir_all(&taskdir).unwrap();
    fs::write(
        taskdir.join("prompt.md"),
        "Produce an executive summary as summary.md",
    )
    .unwrap();
    let pack = root.join("pack.toml");
    fs::write(
        &pack,
        r#"
[pack]
name = "test/pack"
version = "0.1.0"
license = "Apache-2.0"
domain = "demo"

[[task]]
id = "summarize"
prompt = "t1/prompt.md"
tools = ["filesystem"]
expected = { artifacts = ["summary.md"] }
"#,
    )
    .unwrap();
    root.to_path_buf()
}

fn make_manifest(pack: &Path) -> Manifest {
    let toml = format!(
        r#"
[project]
name = "tunertest"

# the old [tasks] name for [evals] still parses
[tasks]
seeds = 2

[tasks.packs]
# (empty, replaced below)

[candidates.llm.planner]
any_of = ["reference:qwen-7b", "reference:qwen-32b"]

[candidates]
decision = ["none", "rules:default"]
harness = ["reference"]

[objective]
primary = "success-rate"
select = "min-cost"

[objective.constraints]
success-rate = ">=0.5"

[budget]
strategy = "grid"
max_trials = 200

[backend]
target = "local"
"#
    );
    let mut m = Manifest::from_str(&toml).unwrap();
    m.evals.insert(
        "packs".to_string(),
        serde_json::json!([pack.to_string_lossy().to_string()]),
    );
    m
}

#[test]
fn tune_writes_a_resolve() {
    let base = std::env::temp_dir().join(format!("hlock-tuner-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).unwrap();
    let cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(&base).unwrap();

    let pack = make_pack(&base);
    let manifest = make_manifest(&pack);
    let mh = manifest.manifest_hash();
    let mut tuner = Tuner::new(manifest, base.join(".harnesslock"));
    let lock = tuner.tune().unwrap();

    // lock is written at cwd
    let lock_path = base.join("harness.lock");
    assert!(lock_path.exists(), "harness.lock should be written at cwd");
    assert_eq!(lock.harness.name, "reference");
    assert!(lock.evidence.trials >= 2);

    // reload and confirm it round-trips
    let reloaded = harness_lock_core::Lockfile::load(&lock_path).unwrap();
    assert_eq!(reloaded.summary(), lock.summary());
    assert!(reloaded.check_manifest(&mh).is_empty());

    std::env::set_current_dir(&cwd).unwrap();
    let _ = fs::remove_dir_all(&base);
}

#[test]
fn manifest_accepts_harness_list() {
    let m = Manifest::from_str(
        r#"
[project]
name = "a"
[candidates.llm.planner]
any_of = ["reference:qwen-7b"]
[candidates]
harness = ["reference"]
"#,
    )
    .unwrap();
    assert_eq!(m.candidates.harness.len(), 1);
    assert_eq!(m.candidates.harness[0].name, "reference");
}
