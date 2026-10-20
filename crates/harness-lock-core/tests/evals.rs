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
