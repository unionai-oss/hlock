//! Eval packs: `EvalPack::check`, and the old `[tasks]` / `task-pack` names.

use std::fs;
use std::path::{Path, PathBuf};

use harness_lock_core::tasks::{EvalPack, PLACEHOLDER};
use harness_lock_core::{Lockfile, Manifest};

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hlock-evals-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}

fn messages(pack: &EvalPack, level: &str) -> Vec<String> {
    pack.check().into_iter().filter(|i| i.level == level).map(|i| i.message).collect()
}

#[test]
fn a_complete_pack_has_no_errors() {
    let root = &tmp("ok");
    write(root, "pack.toml", r#"
[pack]
name = "t/ok"
version = "0.1.0"
domain = "test"

[[task]]
id = "a"
prompt = "a/prompt.md"
expected = { artifacts = ["out.txt"] }
"#);
    write(root, "a/prompt.md", "Write out.txt.\n");
    let pack = EvalPack::load(root.to_str().unwrap(), None).unwrap();
    assert!(messages(&pack, "error").is_empty(), "{:?}", pack.check());
    // one task is too few for a meaningful holdout split
    assert!(messages(&pack, "warn").iter().any(|m| m.contains("holdout")));
}

#[test]
fn check_finds_placeholders_and_broken_tasks() {
    let root = &tmp("broken");
    write(root, "pack.toml", &format!(r#"
[pack]
name = "t/broken"
version = "0.1.0"
domain = "{PLACEHOLDER}: name the domain"

[[task]]
id = "a"
prompt = "a/prompt.md"
fixtures = "a/workspace/"
expected = {{ artifacts = ["out.txt"] }}

[[task]]
id = "a"
prompt = "missing.md"

[[task]]
id = "c"
prompt = "c/prompt.md"
fixtures = "c/nope/"
"#));
    write(root, "a/prompt.md", &format!("{PLACEHOLDER}: describe the task\n"));
    write(root, "a/workspace/out.txt", "already here\n");
    write(root, "c/prompt.md", "Do c.\n");
    let pack = EvalPack::load(root.to_str().unwrap(), None).unwrap();
    let errors = messages(&pack, "error");
    let warns = messages(&pack, "warn");
    assert!(errors.iter().any(|m| m == "pack.toml still has 1 TODO(hlock) (line 5)"), "{errors:?}");
    assert!(errors.iter().any(|m| m == "a/prompt.md still has 1 TODO(hlock)"), "{errors:?}");
    assert!(errors.iter().any(|m| m.contains("duplicate task id")), "{errors:?}");
    assert!(errors.iter().any(|m| m == "prompt file missing.md not found"), "{errors:?}");
    assert!(errors.iter().any(|m| m.contains("fixtures directory c/nope/ not found")), "{errors:?}");
    assert!(warns.iter().any(|m| m.contains("already in the fixtures")), "{warns:?}");
    assert!(warns.iter().any(|m| m.contains("always passes")), "{warns:?}");
}

#[test]
fn task_ids_and_pack_paths_must_stay_inside() {
    let pack_with = |name: &str, task: &str| {
        let root = tmp(name);
        write(&root, "pack.toml", &format!("[pack]\nname = \"t/x\"\nversion = \"0.1.0\"\ndomain = \"test\"\n\n[[task]]\n{task}\n"));
        write(&root, "a/prompt.md", "Do it.\n");
        EvalPack::load(root.to_str().unwrap(), None).map(|_| ()).map_err(|e| e.to_string())
    };
    for (n, id) in ["/tmp/x", "../x", "..", "a/b", "a b", "a\\\\b"].iter().enumerate() {
        let err = pack_with(&format!("id{n}"), &format!("id = \"{id}\"\nprompt = \"a/prompt.md\"")).unwrap_err();
        assert!(err.contains("may only have letters, digits"), "{id}: {err}");
    }
    assert!(pack_with("idok", "id = \"fix_off-by-one.v2\"\nprompt = \"a/prompt.md\"").is_ok());
    for (n, line) in ["prompt = \"/etc/passwd\"", "prompt = \"../outside.md\"", "prompt = \"a/prompt.md\"\nfixtures = \"/\"", "prompt = \"a/prompt.md\"\nsolution = \"a/../../x\"", "prompt = \"a/prompt.md\"\nexpected = { artifacts = [\"/etc/hosts\"] }"].iter().enumerate() {
        let err = pack_with(&format!("path{n}"), &format!("id = \"a\"\n{line}")).unwrap_err();
        assert!(err.contains("must be a relative path inside"), "{line}: {err}");
    }
}

#[test]
fn split_handles_empty_packs_and_out_of_range_fractions() {
    let root = &tmp("split");
    write(root, "pack.toml", "[pack]\nname = \"t/s\"\nversion = \"0.1.0\"\ndomain = \"test\"\n");
    let empty = EvalPack::load(root.to_str().unwrap(), None).unwrap();
    assert_eq!(empty.split(0.7, 0).0.len() + empty.split(0.7, 0).1.len(), 0);
    let mut pack = empty.clone();
    pack.tasks = (0..4).map(|i| harness_lock_core::tasks::Task { id: format!("t{i}"), ..Default::default() }).collect();
    for (frac, search) in [(1.5, 4), (-1.0, 1), (f64::NAN, 4), (0.5, 2)] {
        let (s, h) = pack.split(frac, 7);
        assert_eq!((s.len(), h.len()), (search, 4 - search), "{frac}");
    }
}

#[test]
fn the_old_names_still_load() {
    let m = Manifest::from_str("[tasks]\npacks = [\"./evals\"]\nseeds = 4\n").unwrap();
    assert_eq!(m.eval_packs(), vec!["./evals".to_string()]);
    assert_eq!(m.seeds(), 4);
    let m = Manifest::from_str("[evals]\npacks = [\"./evals\"]\n[evals.splits]\nsearch = 0.5\n").unwrap();
    assert_eq!(m.eval_packs(), vec!["./evals".to_string()]);
    assert_eq!(m.search_frac(), 0.5);

    let path = tmp("names").join("harness.lock");
    let mut lf = Lockfile::default();
    lf.evidence.eval_pack = Some("./evals".into());
    lf.save(&path).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("eval-pack = \"./evals\""), "{text}");
    fs::write(&path, text.replace("eval-pack =", "task-pack =")).unwrap();
    assert_eq!(Lockfile::load(&path).unwrap().evidence.eval_pack.as_deref(), Some("./evals"));
}

fn hello_pack(name: &str) -> PathBuf {
    let root = tmp(name);
    write(&root, "pack.toml", r#"
[pack]
name = "t/keys"
version = "0.1.0"
domain = "test"

[[task]]
id = "a"
prompt = "a/prompt.md"
fixtures = "a/workspace/"
expected = { artifacts = ["out.txt"] }
"#);
    write(&root, "a/prompt.md", "Write out.txt.\n");
    write(&root, "a/workspace/in.txt", "input\n");
    root
}

#[test]
fn check_reports_unknown_keys_with_the_closest_known_one() {
    let root = &hello_pack("keys");
    let text = fs::read_to_string(root.join("pack.toml")).unwrap()
        .replace("domain = \"test\"", "domain = \"test\"\nlicence = \"MIT\"\n\n[pack.judge]\nmodle = \"x\"")
        .replace("fixtures =", "timeout_seconds = 5\ngrader = []\nfixtures =");
    write(root, "pack.toml", &text);
    let pack = EvalPack::load(root.to_str().unwrap(), None).unwrap();
    let errors = messages(&pack, "error");
    for want in [
        "unknown key `licence` in [pack] (did you mean `license`?)",
        "unknown key `modle` in [pack.judge] (did you mean `model`?)",
        "unknown key `timeout_seconds` in [[task]] (did you mean `timeout-seconds`?)",
        "unknown key `grader` in [[task]] (did you mean `graders`?)",
    ] {
        assert!(errors.iter().any(|e| e == want), "{want:?} not in {errors:?}");
    }
    // nothing close: no guess
    write(root, "pack.toml", &text.replace("grader = []", "flavor = \"x\""));
    let pack = EvalPack::load(root.to_str().unwrap(), None).unwrap();
    assert!(messages(&pack, "error").iter().any(|e| e == "unknown key `flavor` in [[task]]"));
}

#[test]
fn the_pack_hash_covers_every_file() {
    let root = &hello_pack("hash");
    let before = EvalPack::load(root.to_str().unwrap(), None).unwrap().hash;
    write(root, "a/prompt.md", "Write out.txt, carefully.\n");
    let after_prompt = EvalPack::load(root.to_str().unwrap(), None).unwrap().hash;
    write(root, "a/workspace/in.txt", "other input\n");
    let after_fixture = EvalPack::load(root.to_str().unwrap(), None).unwrap().hash;
    assert!(before != after_prompt && after_prompt != after_fixture);
}

#[test]
fn prepare_workspace_copies_the_fixtures_into_a_fresh_directory() {
    let root = &hello_pack("prepare");
    let pack = EvalPack::load(root.to_str().unwrap(), None).unwrap();
    let ws = tmp("prepare-ws").join("a-0");
    let task = pack.prepare_workspace("a", &ws).unwrap();
    assert_eq!(task.prompt_text, "Write out.txt.\n");
    assert_eq!(fs::read_to_string(ws.join("in.txt")).unwrap(), "input\n");
    // never over an earlier run
    assert!(pack.prepare_workspace("a", &ws).unwrap_err().to_string().contains("isn't empty"));
    assert!(pack.prepare_workspace("nope", &ws.with_file_name("x")).unwrap_err().to_string().contains("no task \"nope\""));
}
