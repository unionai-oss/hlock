//! End-to-end resolver test: build a temp eval pack, run `Resolver::resolve`, and check
//! the produced `harness.lock`.

use std::fs;
use std::path::{Path, PathBuf};

use harness_lock_core::{Manifest, Resolver};

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
name = "resolvertest"

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
    let base = std::env::temp_dir().join(format!("hlock-resolver-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(&base).unwrap();
    let cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(&base).unwrap();

    let pack = make_pack(&base);
    let manifest = make_manifest(&pack);
    let mh = manifest.manifest_hash();
    let mut resolver = Resolver::new(manifest, base.join(".harnesslock"));
    let lock = resolver.resolve().unwrap();

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

fn pack_with_graders(root: &Path, judge: bool) -> PathBuf {
    fs::create_dir_all(root.join("count/solution")).unwrap();
    fs::write(root.join("count/prompt.md"), "Write the word count to count.txt.\n").unwrap();
    fs::write(root.join("count/solution/count.txt"), "9\n").unwrap();
    let mut toml = String::from("[pack]\nname = \"t/graded\"\nversion = \"0.1.0\"\ndomain = \"demo\"\n");
    if judge {
        // a judge that can't be reached: grades fail, but the evidence names it
        toml.push_str("\n[pack.judge]\nmodel = \"local:judge-1\"\nbase-url = \"http://127.0.0.1:9\"\n");
    }
    toml.push_str("\n[[task]]\nid = \"count\"\nprompt = \"count/prompt.md\"\nsolution = \"count/solution/\"\nexpected = { artifacts = [\"count.txt\"] }\n");
    toml.push_str(if judge {
        "graders = [{ type = \"artifact\", path = \"count.txt\", equals = \"9\" }, { type = \"judge\", criteria = [\"Is a number\"] }]\n"
    } else {
        "graders = [{ type = \"artifact\", path = \"count.txt\", equals = \"9\" }]\n"
    });
    fs::write(root.join("pack.toml"), toml).unwrap();
    root.to_path_buf()
}

fn files_containing(dir: &Path, needle: &str) -> usize {
    let Ok(rd) = fs::read_dir(dir) else { return 0 };
    rd.flatten()
        .map(|e| {
            let p = e.path();
            if p.is_dir() { files_containing(&p, needle) } else { fs::read_to_string(&p).map(|t| t.contains(needle) as usize).unwrap_or(0) }
        })
        .sum()
}

#[test]
fn content_graders_pass_on_the_reference_harness_and_judges_reach_the_evidence() {
    let base = std::env::temp_dir().join(format!("hlock-resolver-graded-{}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    for judge in [false, true] {
        let root = base.join(if judge { "judged" } else { "plain" });
        let pack = pack_with_graders(&root, judge);
        let mut m = make_manifest(&pack);
        m.candidates.llm.get_mut("planner").unwrap().any_of = vec!["reference:qwen-32b".into()];
        m.candidates.decision = vec!["none".into()];
        let mut resolver = Resolver::new(m, root.join(".harnesslock"));
        let lock = resolver.resolve_lock().unwrap();
        if judge {
            assert_eq!(lock.evidence.judge.as_deref(), Some("local:judge-1"));
            assert_eq!(lock.evidence.success_rate["mean"], 0.0, "the unreachable judge fails every trial");
            assert!(files_containing(&root.join(".harnesslock/store"), "cannot reach") > 0, "grade details go in the traces");
        } else {
            assert_eq!(lock.evidence.judge, None);
            // the reference harness left the solution, so `equals = "9"` passed
            assert_eq!(lock.evidence.success_rate["mean"], 1.0);
            assert!(files_containing(&root.join(".harnesslock/store"), "\"grader\":\"artifact:count.txt\"") > 0);
        }
    }
    let _ = fs::remove_dir_all(&base);
}
