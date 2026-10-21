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
target = "offline"
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
    let mut resolver = Resolver::new(manifest, base.join(".hlock"));
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
        if judge {
            // every trial fails, so no stack meets `success-rate >= 0.5`: that's an error
            let err = Resolver::new(m.clone(), root.join(".hlock")).resolve_lock().unwrap_err().to_string();
            assert!(err.contains("no stack meets [objective.constraints]") && err.contains("missing success-rate >=0.5"), "{err}");
            m.objective.constraints.clear();
        }
        let mut resolver = Resolver::new(m, root.join(".hlock"));
        let lock = resolver.resolve_lock().unwrap();
        if judge {
            assert_eq!(lock.evidence.judge.as_ref().map(|j| j.model.as_str()), Some("local:judge-1"));
            assert_eq!(lock.evidence.success_rate["mean"], 0.0, "the unreachable judge fails every trial");
            assert!(files_containing(&root.join(".hlock/store"), "cannot reach") > 0, "grade details go in the traces");
        } else {
            assert_eq!(lock.evidence.judge, None);
            // the reference harness left the solution, so `equals = "9"` passed
            assert_eq!(lock.evidence.success_rate["mean"], 1.0);
            assert!(files_containing(&root.join(".hlock/store"), "\"grader\":\"artifact:count.txt\"") > 0);
        }
    }
    let _ = fs::remove_dir_all(&base);
}

/// Answer every System One request: each noul gets `noul`.
fn fake_systemone(noul: f64) -> String {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { continue };
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut len = 0;
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0; len];
            let _ = r.read_exact(&mut body);
            let body: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
            let answers: serde_json::Map<String, serde_json::Value> = body["questions"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(k, _)| (k.clone(), serde_json::json!({"type": "noul", "noul": noul})))
                .collect();
            let text = serde_json::json!({"model": "kev-4b-q8", "answers": answers, "usage": {"input_tokens": 10}}).to_string();
            let _ = write!(s, "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}", text.len());
        }
    });
    url
}

#[test]
fn deciders_are_searched_locked_and_judge_the_evidence() {
    let url = fake_systemone(0.75);
    let root = std::env::temp_dir().join(format!("hlock-resolver-deciders-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("count/solution")).unwrap();
    fs::write(root.join("count/prompt.md"), "Summarize it in s.md.\n").unwrap();
    fs::write(root.join("count/solution/s.md"), "A summary.\n").unwrap();
    fs::write(root.join("pack.toml"), format!(
        "[pack]\nname = \"t/d\"\nversion = \"0.1.0\"\ndomain = \"demo\"\n\n[pack.judge]\nmodel = \"system-one:kev-4b\"\nbase-url = \"{url}\"\n\n[[task]]\nid = \"count\"\nprompt = \"count/prompt.md\"\nsolution = \"count/solution/\"\nexpected = {{ artifacts = [\"s.md\"] }}\ngraders = [{{ type = \"judge\", criteria = [\"Is a summary\"] }}]\n"
    )).unwrap();
    fs::write(root.join("policy.md"), "Never push.\n").unwrap();

    let mut m = make_manifest(&root);
    m.candidates.llm.get_mut("planner").unwrap().any_of = vec!["reference:qwen-32b".into()];
    m.candidates.decision = vec!["none".into()];
    m.source_path = Some(root.join("harness.toml"));
    // any-of multiplies the stacks; `none` leaves the use out
    m.deciders.insert("approve".into(), serde_json::json!({"any-of": ["none", "system-one:kev-4b"], "base-url": url}));
    let resolver = Resolver::new(m.clone(), root.join(".hlock"));
    assert_eq!(resolver.enumerate_stacks().unwrap().len(), 2);

    m.deciders.insert("approve".into(), serde_json::json!({"model": "system-one:kev-4b", "base-url": url, "policy": "policy.md"}));
    let mut resolver = Resolver::new(m, root.join(".hlock"));
    let lock = resolver.resolve_lock().unwrap();
    assert_eq!(lock.decider.len(), 1, "{:?}", lock.decider);
    let d = &lock.decider[0];
    assert_eq!((d.use_.as_str(), d.via.as_str(), d.model.as_str(), d.endpoint.as_deref()), ("approve", "simulated", "kev-4b", Some(url.as_str())));
    assert_eq!(d.may, ["deny", "ask"]);
    assert!(d.policy.as_ref().is_some_and(|p| p.path == "policy.md"));
    let judge = lock.evidence.judge.clone().expect("the judge reaches the evidence");
    assert_eq!((judge.model.as_str(), judge.answered.as_deref(), judge.protocol.as_deref()), ("system-one:kev-4b", Some("kev-4b-q8"), Some("systemone")));
    // P=0.75 against pass 0.7 is within 0.15: every judge grade was uncertain
    assert_eq!(judge.uncertain_grades, Some(1.0));
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn thresholds_multiply_the_stacks_and_judging_costs_reach_the_evidence() {
    let url = fake_systemone(0.95);
    let root = std::env::temp_dir().join(format!("hlock-resolver-thr-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("t/solution")).unwrap();
    fs::write(root.join("t/prompt.md"), "Write s.md.\n").unwrap();
    fs::write(root.join("t/solution/s.md"), "x\n").unwrap();
    // a judge on TypeSafe's price list, served by the fake
    std::env::set_var("HLOCK_TEST_TYPESAFE_KEY", "k");
    fs::write(root.join("pack.toml"), format!(
        "[pack]\nname = \"t/thr\"\nversion = \"0.1.0\"\ndomain = \"demo\"\n\n[pack.judge]\nmodel = \"typesafe:jev-1.13.0\"\nbase-url = \"{url}\"\nkey-env = \"HLOCK_TEST_TYPESAFE_KEY\"\n\n[[task]]\nid = \"t\"\nprompt = \"t/prompt.md\"\nsolution = \"t/solution/\"\nexpected = {{ artifacts = [\"s.md\"] }}\ngraders = [{{ type = \"judge\", criteria = [\"Is there\"] }}]\n"
    )).unwrap();
    let mut m = make_manifest(&root);
    m.candidates.llm.get_mut("planner").unwrap().any_of = vec!["reference:qwen-32b".into()];
    m.candidates.decision = vec!["none".into()];
    m.source_path = Some(root.join("harness.toml"));
    m.deciders.insert("stop".into(), serde_json::json!({"model": "system-one:kev-4b", "base-url": url, "min-probability": [0.6, 0.9]}));
    let resolver = Resolver::new(m.clone(), root.join(".hlock"));
    let stacks = resolver.enumerate_stacks().unwrap();
    let tried: Vec<&String> = stacks.iter().map(|s| &s.deciders["stop"]).collect();
    assert_eq!(tried, ["system-one:kev-4b@0.6", "system-one:kev-4b@0.9"]);
    // candidates keep the manifest's order (it decides which stacks a 200-stack cap keeps)
    let mut m2 = m.clone();
    m2.deciders.insert("stop".into(), serde_json::json!({"any-of": ["system-one:kev-9b", "none", "system-one:kev-4b"], "base-url": url}));
    let order: Vec<String> = Resolver::new(m2, root.join(".hlock")).enumerate_stacks().unwrap().iter().map(|s| s.deciders.get("stop").cloned().unwrap_or_else(|| "none".into())).collect();
    assert_eq!(order, ["system-one:kev-9b", "none", "system-one:kev-4b"]);

    let mut resolver = Resolver::new(m, root.join(".hlock"));
    let lock = resolver.resolve_lock().unwrap();
    let d = &lock.decider[0];
    assert!(d.min_probability == serde_json::json!(0.6) || d.min_probability == serde_json::json!(0.9), "{d:?}");
    assert_eq!(d.model, "kev-4b", "the threshold isn't part of the model");
    // 10 input tokens a judging at $0.042 per million
    let judge = lock.evidence.judge.unwrap();
    assert_eq!(judge.cost_usd_per_task, Some(0.00000042));
    let _ = fs::remove_dir_all(&root);
}

fn tmp(name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!("hlock-resolver-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    root
}

#[test]
fn the_judge_evidence_describes_only_this_resolve() {
    let url = fake_systemone(0.95);
    let root = tmp("judge-again");
    fs::create_dir_all(root.join("t/solution")).unwrap();
    fs::write(root.join("t/prompt.md"), "Write s.md.\n").unwrap();
    fs::write(root.join("t/solution/s.md"), "x\n").unwrap();
    fs::write(root.join("pack.toml"), format!(
        "[pack]\nname = \"t/j\"\nversion = \"0.1.0\"\ndomain = \"demo\"\n\n[pack.judge]\nmodel = \"system-one:kev-4b\"\nbase-url = \"{url}\"\n\n[[task]]\nid = \"t\"\nprompt = \"t/prompt.md\"\nsolution = \"t/solution/\"\nexpected = {{ artifacts = [\"s.md\"] }}\ngraders = [{{ type = \"judge\", criteria = [\"Is there\"] }}]\n"
    )).unwrap();
    let mut m = make_manifest(&root);
    m.candidates.llm.get_mut("planner").unwrap().any_of = vec!["reference:qwen-32b".into()];
    m.candidates.decision = vec!["none".into()];
    let mut resolver = Resolver::new(m, root.join(".hlock"));
    assert_eq!(resolver.resolve_lock().unwrap().evidence.judge.unwrap().model, "system-one:kev-4b");
    // a second resolve with another judge doesn't carry the first one's over
    resolver.manifest.evals.insert("judge".into(), serde_json::json!({"model": "system-one:kev-9b", "base-url": url}));
    assert_eq!(resolver.resolve_lock().unwrap().evidence.judge.unwrap().model, "system-one:kev-9b");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_bad_deciders_table_or_an_unlockable_decider_is_an_error() {
    let root = tmp("bad-deciders");
    let pack = make_pack(&root);
    let mut m = make_manifest(&pack);
    m.source_path = Some(root.join("harness.toml"));
    m.deciders.insert("approve".into(), serde_json::json!({"model": 42}));
    let err = Resolver::new(m.clone(), root.join(".hlock")).resolve_lock().unwrap_err().to_string();
    assert!(err.contains("[deciders.approve]"), "{err}");
    // a policy file that isn't there can't be locked, so it can't be dropped from the lock either
    m.deciders.insert("approve".into(), serde_json::json!({"model": "system-one:kev-4b", "base-url": "http://127.0.0.1:9", "policy": "missing.md"}));
    let err = Resolver::new(m, root.join(".hlock")).resolve_lock().unwrap_err().to_string();
    assert!(err.contains("policy missing.md"), "{err}");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn max_trials_is_spread_across_the_stacks() {
    let root = tmp("budget");
    let pack = make_pack(&root);
    let mut m = make_manifest(&pack);
    m.candidates.decision = vec!["none".into()];
    m.objective.constraints.clear();
    // 2 stacks x 1 task x 2 seeds is 4 trials; the budget allows 3
    m.budget.max_trials = Some(3);
    let mut resolver = Resolver::new(m, root.join(".hlock"));
    let hashes: Vec<String> = resolver.enumerate_stacks().unwrap().iter().map(|s| s.hash()).collect();
    resolver.resolve_lock().unwrap();
    let store = root.join(".hlock/store");
    let per_stack: Vec<usize> = hashes.iter().map(|h| files_containing(&store, &format!("\"stack\":\"{h}\""))).collect();
    assert_eq!(per_stack.iter().sum::<usize>(), 3, "no trial runs past the budget: {per_stack:?}");
    assert!(per_stack.iter().all(|n| *n >= 1), "every stack gets trials: {per_stack:?}");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn a_pack_with_no_tasks_is_an_error() {
    let root = tmp("empty");
    fs::write(root.join("pack.toml"), "[pack]\nname = \"t/e\"\nversion = \"0.1.0\"\ndomain = \"demo\"\n").unwrap();
    let err = Resolver::new(make_manifest(&root), root.join(".hlock")).resolve_lock().unwrap_err().to_string();
    assert!(err.contains("no tasks to measure the stacks on"), "{err}");
    let _ = fs::remove_dir_all(&root);
}

#[test]
fn bad_constraints_are_an_error() {
    let root = tmp("constraints");
    let pack = make_pack(&root);
    let mut m = make_manifest(&pack);
    m.objective.constraints.insert("success-rate".into(), ">= high".into());
    let err = Resolver::new(m.clone(), root.join(".hlock")).resolve_lock().unwrap_err().to_string();
    assert!(err.contains("success-rate = \">= high\"") && err.contains("isn't a number"), "{err}");
    m.objective.constraints.clear();
    m.objective.constraints.insert("latency".into(), "<1".into());
    let err = Resolver::new(m, root.join(".hlock")).resolve_lock().unwrap_err().to_string();
    assert!(err.contains("unknown metric"), "{err}");
    let _ = fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn a_stack_whose_every_trial_fails_to_run_fails_the_resolve() {
    use std::os::unix::fs::PermissionsExt;
    let root = tmp("broken");
    let pack = make_pack(&root);
    let text = fs::read_to_string(root.join("pack.toml")).unwrap().replace("tools = [\"filesystem\"]", "fixtures = \"t1/ws/\"");
    fs::write(root.join("pack.toml"), text).unwrap();
    // a fixture the harness can't copy
    fs::create_dir_all(root.join("t1/ws")).unwrap();
    fs::write(root.join("t1/ws/locked.txt"), "x").unwrap();
    fs::set_permissions(root.join("t1/ws/locked.txt"), fs::Permissions::from_mode(0o000)).unwrap();
    if fs::read(root.join("t1/ws/locked.txt")).is_ok() {
        return; // running as root: nothing is unreadable
    }
    let err = Resolver::new(make_manifest(&pack), root.join(".hlock")).resolve_lock().unwrap_err().to_string();
    assert!(err.contains("every trial of") && err.contains("failed to run"), "{err}");
    let store = root.join(".hlock/store");
    assert!(files_containing(&store, "\"error\":") > 0, "the error is in the trial's trace");
    fs::set_permissions(root.join("t1/ws/locked.txt"), fs::Permissions::from_mode(0o644)).unwrap();
    let _ = fs::remove_dir_all(&root);
}
