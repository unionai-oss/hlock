//! Decision models: the client (both protocols), judges and trace graders on deciders,
//! calibration, `[deciders.*]` and `[[decider]]`, and the hooks `sync` wires in.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use harness_lock_core::decide::{self, DeciderConfig, DeciderSpec, Question};
use harness_lock_core::lockfile::{Decider, Lockfile, Wiring};
use harness_lock_core::tasks::{EvalPack, GradeOptions, JudgeConfig, Judges};
use serde_json::{json, Value};

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hlock-deciders-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}

/// A fake model server: each request's path and JSON body is recorded, and `answer`
/// gives the status and body to reply with.
struct Fake {
    url: String,
    requests: Arc<Mutex<Vec<(String, Value)>>>,
}

impl Fake {
    fn start(answer: impl Fn(&str, &Value, usize) -> (u16, Value) + Send + 'static) -> Fake {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let requests: Arc<Mutex<Vec<(String, Value)>>> = Arc::default();
        let log = requests.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut first = String::new();
                if r.read_line(&mut first).is_err() {
                    continue;
                }
                let path = first.split_whitespace().nth(1).unwrap_or("/").to_string();
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
                let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
                let n = {
                    let mut l = log.lock().unwrap();
                    l.push((path.clone(), body.clone()));
                    l.len()
                };
                let (status, reply) = answer(&path, &body, n);
                let text = reply.to_string();
                let _ = write!(s, "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}", text.len());
            }
        });
        Fake { url, requests }
    }

    fn requests(&self) -> Vec<(String, Value)> {
        self.requests.lock().unwrap().clone()
    }
}

/// A System One reply: every noul gets `noul`, every score gets level `level` of its
/// levels, every choice gets `choice` with `p`.
fn systemone(body: &Value, noul: f64, level: usize, choice: &str, p: f64) -> Value {
    let mut answers = serde_json::Map::new();
    for (id, q) in body["questions"].as_object().unwrap() {
        let a = match q["type"].as_str().unwrap() {
            "noul" => json!({"type": "noul", "noul": noul}),
            "score" => {
                let n = q["criteria"].as_array().unwrap().len();
                let probs: serde_json::Map<String, Value> = (0..n).map(|i| (i.to_string(), json!(if i == level { 1.0 } else { 0.0 }))).collect();
                json!({"type": "score", "score": level, "probabilities": probs, "confidence": 0.9})
            }
            _ => {
                let others: Vec<String> = q["criteria"].as_object().unwrap().keys().filter(|k| *k != choice).cloned().collect();
                let mut probs = serde_json::Map::new();
                probs.insert(choice.into(), json!(p));
                for o in &others {
                    probs.insert(o.clone(), json!((1.0 - p) / others.len() as f64));
                }
                json!({"type": "choice", "choice": choice, "probabilities": probs, "confidence": p})
            }
        };
        answers.insert(id.clone(), a);
    }
    json!({"model": format!("{}-1.0", body["model"].as_str().unwrap_or("m")), "answers": answers, "usage": {"input_tokens": 100, "output_tokens": 0}})
}

fn cfg(model: &str, url: &str) -> DeciderConfig {
    DeciderConfig { model: model.into(), base_url: Some(url.into()), ..Default::default() }
}

fn no_env(_: &str) -> Option<String> {
    None
}

// --------------------------------------------------------------------------- //
// The client
// --------------------------------------------------------------------------- //

#[test]
fn system_one_requests_batch_retry_and_report_the_model() {
    let fake = Fake::start(|_, body, n| if n == 1 { (429, json!({"error": "slow down"})) } else { (200, systemone(body, 0.8, 0, "a", 0.9)) });
    let ep = cfg("system-one:kev-4b", &fake.url).endpoint(&no_env).unwrap();
    let qs: Vec<Question> = (0..65).map(|i| Question::noul(&format!("q{i}"), "Is it so?")).collect();
    let d = decide::decide(&ep, &json!({"a": 1}), &qs).unwrap();
    assert_eq!(d.answers.len(), 65);
    assert_eq!(d.model, "kev-4b-1.0", "the answered model, not the requested one");
    assert_eq!(d.input_tokens, 200);
    let reqs = fake.requests();
    // one 429 retried, then 64 + 1 questions (the System One per-request limit we use)
    assert_eq!(reqs.len(), 3, "{reqs:?}");
    assert!(reqs.iter().all(|(p, _)| p == "/v1/systemone"));
    assert_eq!(reqs[1].1["questions"].as_object().unwrap().len(), 64);
    assert_eq!(reqs[2].1["questions"].as_object().unwrap().len(), 1);
    assert_eq!(reqs[1].1["state"], json!({"a": 1}));
}

#[test]
fn decisions_protocol_end_to_end() {
    let fake = Fake::start(|_, body, _| {
        let n = body["questions"].as_array().unwrap().len();
        let answers: Vec<Value> = (0..n).map(|i| json!({"type": "predicate", "name": format!("q{i}"), "probability": 0.25})).collect();
        (200, json!({"model": "gpt-6-luna-2026-10", "answers": answers, "usage": {"input_tokens": 50}}))
    });
    let mut c = cfg("openai-decisions:gpt-6-luna", &format!("{}/v1", fake.url));
    c.key_env = Some("HLOCK_TEST_DECISIONS_KEY".into());
    let env = |k: &str| (k == "HLOCK_TEST_DECISIONS_KEY").then(|| "sk".to_string());
    let ep = c.endpoint(&env).unwrap();
    let d = decide::decide(&ep, &json!("some text"), &[Question::noul("q0", "Is it?")]).unwrap();
    assert_eq!((d.answers[0].probability(), d.model.as_str()), (0.25, "gpt-6-luna-2026-10"));
    assert_eq!(d.cost_usd, Some(50.0 * 0.10 / 1e6));
    let (path, body) = &fake.requests()[0];
    assert_eq!(path, "/v1/decisions");
    assert_eq!((body["input"].as_str(), body["questions"][0]["type"].as_str()), (Some("some text"), Some("predicate")));
}

#[test]
fn errors_say_what_went_wrong() {
    let fake = Fake::start(|_, _, _| (401, json!({"error": {"message": "bad key"}})));
    let ep = cfg("system-one:x", &fake.url).endpoint(&no_env).unwrap();
    let e = decide::decide(&ep, &json!({}), &[Question::noul("q", "?")]).unwrap_err();
    assert!(e.ends_with("returned 401: bad key"), "{e}");
    let ep = cfg("system-one:x", "http://127.0.0.1:9").endpoint(&no_env).unwrap();
    assert!(decide::decide(&ep, &json!({}), &[Question::noul("q", "?")]).unwrap_err().starts_with("cannot reach"));
}

// --------------------------------------------------------------------------- //
// Judges and trace graders on deciders
// --------------------------------------------------------------------------- //

fn pack(name: &str, task: &str, pack_extra: &str) -> (PathBuf, EvalPack) {
    let root = tmp(name);
    write(&root, "pack.toml", &format!(
        "[pack]\nname = \"t/{name}\"\nversion = \"0.1.0\"\ndomain = \"test\"\n{pack_extra}\n[[task]]\nid = \"a\"\nprompt = \"a/prompt.md\"\n{task}\n"
    ));
    write(&root, "a/prompt.md", "Summarize the Q3 report in summary.md.\n");
    let p = EvalPack::load(root.to_str().unwrap(), None).unwrap();
    (root, p)
}

fn with_judge(model: &str, url: &str) -> GradeOptions {
    GradeOptions { judge: Some(JudgeConfig { model: model.into(), base_url: Some(url.into()), ..Default::default() }), ..Default::default() }
}

#[test]
fn a_decider_judge_asks_one_noul_per_criterion_and_a_score_for_levels() {
    let fake = Fake::start(|_, body, _| (200, systemone(body, 0.9, 2, "", 0.0)));
    let (root, p) = pack("dj", r#"
expected = { artifacts = ["summary.md"] }
graders = [{ type = "judge", criteria = ["Names revenue", { text = "Cites the report", true = "Names the report", false = "No source" }], levels = ["Off topic", "Partial", "Complete"] }]"#, "");
    let ws = root.join("ws");
    write(&ws, "summary.md", "Revenue was $1.2M (Q3 report).\n");
    let g = p.grade_with(&p.tasks[0], &ws, &with_judge("system-one:kev-4b", &fake.url));
    let body = &fake.requests()[0].1;
    assert_eq!(body["questions"]["c1"], json!({"type": "noul", "instructions": "Names revenue"}));
    assert_eq!(body["questions"]["c2"]["criteria"], json!({"true": "Names the report", "false": "No source"}));
    assert_eq!(body["questions"]["level"]["criteria"], json!(["Off topic", "Partial", "Complete"]));
    assert_eq!(body["state"]["files"]["summary.md"], "Revenue was $1.2M (Q3 report).\n");
    let j = &g[1];
    // criteria mean 0.9, level 2 of 2 → 1.0; the score is the mean of the two
    assert!((j.score - 0.95).abs() < 1e-9 && j.success, "{j:?}");
    assert_eq!(j.grader, "judge:2 criteria (system-one:kev-4b)");
    assert_eq!(j.judged_by.as_deref(), Some("kev-4b-1.0"));
    assert_eq!(j.parts.iter().map(|p| p.label.as_str()).collect::<Vec<_>>(), ["Names revenue", "Cites the report", "level: Complete"]);
    assert!(!j.uncertain);
}

#[test]
fn uncertain_decider_grades_are_marked_and_can_fall_back_to_an_llm() {
    let decider = Fake::start(|_, body, _| (200, systemone(body, 0.62, 0, "", 0.0)));
    let llm = Fake::start(|_, _, _| (200, json!({"choices": [{"message": {"content": "{\"criteria\": [true], \"reason\": \"It names revenue.\"}"}}]})));
    let (root, p) = pack("unsure", r#"
expected = { artifacts = ["summary.md"] }
graders = [{ type = "judge", criteria = ["Names revenue"] }]"#, "");
    let ws = root.join("ws");
    write(&ws, "summary.md", "Revenue rose.\n");
    let g = p.grade_with(&p.tasks[0], &ws, &with_judge("system-one:kev-4b", &decider.url));
    assert!(g[1].uncertain && !g[1].success, "0.62 is within 0.15 of 0.7: {:?}", g[1]);
    assert_eq!(llm.requests().len(), 0);

    let mut opts = with_judge("system-one:kev-4b", &decider.url);
    let j = opts.judge.as_mut().unwrap();
    j.fallback = Some("local:tiny".into());
    j.fallback_base_url = Some(llm.url.clone());
    let g = p.grade_with(&p.tasks[0], &ws, &opts);
    assert!(g[1].success && g[1].uncertain, "the LLM's verdict counts: {:?}", g[1]);
    assert!(g[1].detail.starts_with("uncertain at 0.62 by kev-4b-1.0; re-judged by local:tiny: It names revenue."), "{:?}", g[1]);
    assert_eq!(llm.requests().len(), 1);
    // a sure grade isn't re-judged
    let sure = Fake::start(|_, body, _| (200, systemone(body, 0.97, 0, "", 0.0)));
    let mut opts = with_judge("system-one:kev-4b", &sure.url);
    opts.judge.as_mut().unwrap().fallback = Some("local:tiny".into());
    opts.judge.as_mut().unwrap().fallback_base_url = Some(llm.url.clone());
    assert!(p.grade_with(&p.tasks[0], &ws, &opts)[1].success);
    assert_eq!(llm.requests().len(), 1);
}

#[test]
fn rubric_files_give_deciders_criteria_and_levels() {
    let (c, l) = harness_lock_core::graders::rubric_parts("# Rubric\n\nIntro prose.\n\n- Names revenue\n  - (a nested note)\n* Cites the report\n\n## Levels\n\n0. Off topic\n1. Complete\n");
    assert_eq!(c.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(), ["Names revenue", "Cites the report"]);
    assert_eq!(l, ["Off topic", "Complete"]);

    let fake = Fake::start(|_, body, _| (200, systemone(body, 0.9, 1, "", 0.0)));
    let (root, p) = pack("rubric", r#"
expected = { artifacts = ["summary.md"] }
graders = ["judge:a/rubric.md", "judge:a/freeform.md"]"#, "");
    write(&root, "a/rubric.md", "- Names revenue\n");
    write(&root, "a/freeform.md", "A good summary is short and accurate.\n");
    write(&root, "ws/summary.md", "x\n");
    let g = p.grade_with(&p.tasks[0], &root.join("ws"), &with_judge("system-one:kev-4b", &fake.url));
    assert!(g[1].success, "{:?}", g[1]);
    assert!(g[2].detail.starts_with("a decision model needs criteria or levels"), "{:?}", g[2]);
    // `check` warns about the free-form rubric when the judge is a decider
    let judge = JudgeConfig::new("typesafe:jev-1.13.0");
    let warns: Vec<String> = p.check_with(Some(&judge)).into_iter().map(|i| i.message).collect();
    assert!(warns.iter().any(|m| m.starts_with("a/freeform.md has no criteria")), "{warns:?}");
}

#[test]
fn trace_graders_ask_about_the_tool_calls() {
    let fake = Fake::start(|_, body, _| (200, systemone(body, if body["state"]["tool_calls"][0]["tool"] == "shell" { 0.1 } else { 0.9 }, 0, "", 0.0)));
    let (root, p) = pack("trace", r#"graders = [{ type = "trace", criteria = ["Never ran sudo"], pass = 1.0 }, "trace:Asked before deleting files"]"#, "");
    let trace = json!({"events": [{"type": "tool.call", "tool": "shell", "input": "sudo rm -rf /"}, {"type": "llm.call"}]});
    let mut opts = with_judge("system-one:kev-4b", &fake.url);
    opts.trace = Some(trace);
    let g = p.grade_with(&p.tasks[0], &root, &opts);
    assert_eq!(g.len(), 2, "{g:?}");
    assert_eq!(g[0].grader, "trace:1 criteria (system-one:kev-4b)");
    assert!(!g[0].success && g[0].detail.starts_with("unmet: Never ran sudo"), "{:?}", g[0]);
    let body = &fake.requests()[0].1;
    assert_eq!(body["state"]["tool_calls"], json!([{"tool": "shell", "input": "sudo rm -rf /", "status": null}]));
    // no trace (a directory graded by hand): skipped; an LLM judge: refused
    assert!(p.grade_with(&p.tasks[0], &root, &with_judge("system-one:kev-4b", &fake.url)).is_empty());
    let mut llm = with_judge("anthropic:claude-x", &fake.url);
    llm.trace = Some(json!({"events": []}));
    assert!(p.grade_with(&p.tasks[0], &root, &llm)[0].detail.starts_with("trace graders need a decision model"));
}

#[cfg(unix)]
#[test]
fn tests_graders_get_the_trace() {
    let (root, p) = pack("htrace", r#"graders = [{ type = "tests", command = "grep -q rm \"$HLOCK_TRACE\"" }]"#, "");
    let mut opts = GradeOptions { trace: Some(json!({"events": [{"tool": "shell", "input": "rm x"}]})), ..Default::default() };
    assert!(p.grade_with(&p.tasks[0], &root, &opts)[0].success);
    opts.trace = Some(json!({"events": []}));
    assert!(!p.grade_with(&p.tasks[0], &root, &opts)[0].success);
}

#[test]
fn the_judge_order() {
    let (_, p) = pack("order", r#"expected = { artifacts = ["s.md"] }
graders = [{ type = "judge", criteria = ["x"] }]"#, "\n[pack.judge]\nmodel = \"pack:judge\"\n");
    let m = |s: &str| JudgeConfig::new(s);
    let opts = |judge: Option<&str>, default: Option<&str>| GradeOptions { judge: judge.map(m), default_judge: default.map(m), ..Default::default() };
    assert_eq!(p.judge_for(&opts(Some("evals:judge"), Some("deciders:judge"))).unwrap().model, "evals:judge");
    assert_eq!(p.judge_for(&opts(None, Some("deciders:judge"))).unwrap().model, "pack:judge");
    let (_, bare) = pack("order2", r#"expected = { artifacts = ["s.md"] }"#, "");
    assert_eq!(bare.judge_for(&opts(None, Some("deciders:judge"))).unwrap().model, "deciders:judge");
    assert!(bare.judge_for(&opts(None, None)).is_none(), "auto is off unless asked for");

    let manifest = harness_lock_core::Manifest::from_str("[deciders.default]\nmodel = \"typesafe:jev-1.13.0\"\n[deciders.judge]\nmodel = \"ollama:kev:9b\"\n").unwrap();
    assert_eq!(manifest.default_judge().unwrap().model, "ollama:kev:9b");
    // an alias in a pack is a warning
    let (_, alias) = pack("alias", "", "\n[pack.judge]\nmodel = \"typesafe:jev-latest\"\n");
    assert!(alias.check().iter().any(|i| i.message.contains("is an alias")));
}

#[test]
fn check_run_runs_decider_judges_but_not_llm_judges() {
    let fake = Fake::start(|_, body, _| (200, systemone(body, if body["state"]["files"]["summary.md"].as_str().is_some_and(|s| s.contains("Revenue")) { 0.66 } else { 0.1 }, 0, "", 0.0)));
    let (root, p) = pack("checkrun", r#"
fixtures = "a/workspace/"
solution = "a/solution/"
expected = { artifacts = ["summary.md"] }
graders = [{ type = "judge", criteria = ["Names revenue"], pass = 0.6 }]"#, "");
    write(&root, "a/workspace/report.txt", "Revenue $1.2M\n");
    write(&root, "a/solution/summary.md", "Revenue was $1.2M.\n");
    let p = EvalPack::load(p.path.to_str().unwrap(), None).unwrap();
    let issues: Vec<String> = p.check_graders(&GradeOptions { judges: Judges::Deciders, ..with_judge("system-one:kev-4b", &fake.url) }).into_iter().map(|i| i.message).collect();
    // P=0.66 passes 0.6 but is within 0.15 of it
    assert_eq!(issues, [r#"judge:1 criteria (system-one:kev-4b) is uncertain on its solution: "Names revenue" P=0.66"#]);
    let n = fake.requests().len();
    assert!(n >= 1);
    // an LLM judge doesn't run in check --run without --judges
    let issues = p.check_graders(&GradeOptions { judges: Judges::Deciders, ..with_judge("anthropic:claude-x", &fake.url) });
    assert!(issues.is_empty(), "{issues:?}");
    assert_eq!(fake.requests().len(), n);
}

#[test]
fn calibrate_compares_two_judges_per_criterion() {
    let decider = Fake::start(|_, body, _| (200, systemone(body, 0.9, 0, "", 0.0)));
    let llm = Fake::start(|_, _, _| (200, json!({"choices": [{"message": {"content": "{\"criteria\": [true, false], \"reason\": \"\"}"}}]})));
    let (root, p) = pack("calib", r#"
solution = "a/solution/"
expected = { artifacts = ["summary.md"] }
graders = [{ type = "judge", criteria = ["Names revenue", "Cites the report"] }]"#, "");
    write(&root, "a/solution/summary.md", "Revenue was $1.2M.\n");
    let p = EvalPack::load(p.path.to_str().unwrap(), None).unwrap();
    let a = JudgeConfig { model: "system-one:kev-4b".into(), base_url: Some(decider.url.clone()), ..Default::default() };
    let b = JudgeConfig { model: "local:tiny".into(), base_url: Some(llm.url.clone()), ..Default::default() };
    let rows = p.calibrate(&a, &b);
    let solution: Vec<(String, bool)> = rows.iter().filter(|r| r.run == "solution").map(|r| (r.criterion.clone(), r.agree)).collect();
    assert_eq!(solution, [("Names revenue".to_string(), true), ("Cites the report".to_string(), false), ("(passes)".to_string(), false)]);
    assert!(rows.iter().any(|r| r.run == "fixtures"));
}

// --------------------------------------------------------------------------- //
// [deciders.*] and [[decider]]
// --------------------------------------------------------------------------- //

#[test]
fn decider_specs_are_checked() {
    let spec = |toml: &str| -> DeciderSpec { toml::from_str(toml).unwrap() };
    let ok = spec("model = \"ollama:kev:4b\"").validated("approve").unwrap();
    assert_eq!(ok.may, ["deny", "ask"], "approve may only make the agent more careful by default");
    for (use_, toml, err) in [
        ("nope", "model = \"ollama:kev:4b\"", "unknown use"),
        ("stop", "", "needs a `model` or `any-of`"),
        ("stop", "model = \"a:b\"\nany-of = [\"c:d\"]", "needs a `model` or `any-of`"),
        ("stop", "model = \"anthropic:claude-x\"", "isn't a decision model"),
        ("stop", "model = \"ollama:kev:4b\"\nmay = [\"allow\"]", "`may` is only for approve"),
        ("approve", "model = \"ollama:kev:4b\"\nmay = [\"maybe\"]", "not \"maybe\""),
        ("approve", "model = \"ollama:kev:4b\"\nmin-probability = 2", "`min-probability`"),
        ("stop", "model = \"ollama:kev:4b\"\nmin-probability = { deny = 0.5 }", "`min-probability`"),
    ] {
        let e = if toml.is_empty() { DeciderSpec::default().validated(use_) } else { spec(toml).validated(use_) }.unwrap_err();
        assert!(e.contains(err), "{use_} {toml}: {e}");
    }
    assert!(spec("any-of = [\"none\", \"ollama:kev:4b\"]").validated("stop").is_ok());
    assert!(toml::from_str::<DeciderSpec>("modle = \"x\"").is_err(), "unknown keys are refused");
    let m = harness_lock_core::Manifest::from_str("[deciders.approve]\nmodel = \"anthropic:x\"\n").unwrap();
    assert!(m.deciders().unwrap_err().to_string().contains("isn't a decision model"));
    assert_eq!(decide::min_probability("approve", &json!({"deny": 0.6}), "deny"), 0.6);
    assert_eq!(decide::min_probability("approve", &Value::Null, "allow"), 0.9);
    assert_eq!(decide::min_probability("stop", &json!(0.7), "done"), 0.7);
}

#[test]
fn lock_entries_and_drift() {
    let root = tmp("entry");
    write(&root, ".hlock/approve.md", "Never push to main.\n");
    let spec: DeciderSpec = toml::from_str("model = \"ollama:kev:4b\"\npolicy = \".hlock/approve.md\"\nhooks = [\"PreToolUse\"]").unwrap();
    let spec = spec.validated("approve").unwrap();
    let wiring = vec![Wiring { file: "settings".into(), event: "PermissionRequest".into() }];
    let d = decide::lock_entry("approve", "ollama:kev:4b", &spec, "hook", wiring, &root).unwrap();
    assert_eq!((d.provider.as_str(), d.model.as_str(), d.runtime.as_deref(), d.key_env.as_deref()), ("ollama", "kev:4b", Some("ollama"), None));
    assert_eq!(d.wiring, [Wiring { file: "settings".into(), event: "PreToolUse".into() }], "`hooks` overrides the profile's events");
    assert_eq!(d.questions, decide::questions_digest("approve"));
    let d2 = decide::lock_entry("stop", "typesafe:jev-1.13.0", &toml::from_str::<DeciderSpec>("model = \"typesafe:jev-1.13.0\"").unwrap(), "hook", vec![], &root).unwrap();
    assert_eq!((d2.key_env.as_deref(), d2.endpoint.as_deref()), (Some("TYPESAFE_API_KEY"), Some("https://api.typesafe.ai")));

    let mut lock = Lockfile { decider: vec![d.clone()], ..Default::default() };
    assert!(decide::drift(&lock, &root).is_empty());
    write(&root, ".hlock/approve.md", "Never push to main or release.\n");
    lock.decider[0].questions = Some("sha256:old".into());
    let drift = decide::drift(&lock, &root);
    assert_eq!(drift.len(), 2, "{drift:?}");
    assert!(drift[0].contains("hlock's questions for approve changed") && drift[1].contains("policy .hlock/approve.md changed"));

    // [[decider]] round-trips through the lock file
    let path = root.join("harness.lock");
    lock.save(&path).unwrap();
    let back = Lockfile::load(&path).unwrap();
    assert_eq!(back.decider, lock.decider);
    assert!(fs::read_to_string(&path).unwrap().contains("[[decider]]\nuse = \"approve\""));
}

#[test]
fn evidence_judge_reads_the_old_string_and_the_table() {
    let root = tmp("ev");
    let path = root.join("harness.lock");
    let mut lf = Lockfile::default();
    lf.evidence.judge = Some(harness_lock_core::lockfile::EvidenceJudge { model: "typesafe:jev-1.13.0".into(), answered: Some("jev-1.13.0".into()), protocol: Some("systemone".into()), uncertain_grades: Some(0.04), ..Default::default() });
    lf.save(&path).unwrap();
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("[evidence.judge]\nmodel = \"typesafe:jev-1.13.0\""), "{text}");
    assert_eq!(Lockfile::load(&path).unwrap().evidence.judge.unwrap().uncertain_grades, Some(0.04));
    // locks written before [evidence.judge] have a string
    Lockfile::default().save(&path).unwrap();
    let text = fs::read_to_string(&path).unwrap().replace("[evidence]\n", "[evidence]\njudge = \"anthropic:claude-sonnet-5-5\"\n");
    fs::write(&path, text).unwrap();
    assert_eq!(Lockfile::load(&path).unwrap().evidence.judge.unwrap().model, "anthropic:claude-sonnet-5-5");
}

#[test]
fn code_harnesses_get_decider_variables() {
    let env = |k: &str| (k == "TYPESAFE_API_KEY").then(|| "tk".to_string());
    let vars = decide::decider_env(
        &[("approve".into(), DeciderConfig::from_ref("typesafe:jev-1.13.0")), ("stop".into(), DeciderConfig::from_ref("ollama:kev:4b"))],
        &[("SYSTEM_ONE_BASE_URL".into(), "SYSTEM_ONE_API_KEY".into())],
        &env,
    );
    assert_eq!(vars["HLOCK_DECIDER_APPROVE"], "typesafe:jev-1.13.0");
    assert_eq!(vars["HLOCK_DECIDER_APPROVE_BASE_URL"], "https://api.typesafe.ai");
    assert_eq!(vars["HLOCK_DECIDER_STOP_PROTOCOL"], "systemone");
    assert_eq!((vars["SYSTEM_ONE_BASE_URL"].as_str(), vars["SYSTEM_ONE_API_KEY"].as_str()), ("https://api.typesafe.ai", "tk"));
}

// --------------------------------------------------------------------------- //
// Hooks
// --------------------------------------------------------------------------- //

fn hook_lock(url: &str, use_: &str, may: &[&str], min: Value) -> Lockfile {
    Lockfile {
        decider: vec![Decider {
            use_: use_.into(),
            provider: "system-one".into(),
            model: "kev-4b".into(),
            protocol: "systemone".into(),
            endpoint: Some(url.into()),
            via: "hook".into(),
            wiring: vec![Wiring { file: "settings".into(), event: "PermissionRequest".into() }],
            may: may.iter().map(|s| s.to_string()).collect(),
            min_probability: min,
            ..Default::default()
        }],
        ..Default::default()
    }
}

#[test]
fn approve_hooks_answer_only_when_sure_and_allowed() {
    let root = tmp("hook");
    let ev = |event: &str| json!({"hook_event_name": event, "tool_name": "Bash", "tool_input": {"command": "rm -rf build"}, "cwd": "/p"});
    let run = |choice: &'static str, p: f64, may: &[&str], event: &str, format: &str| {
        let fake = Fake::start(move |_, body, _| (200, systemone(body, 0.0, 0, choice, p)));
        let out = decide::hook("approve", format, &ev(event), &hook_lock(&fake.url, "approve", may, Value::Null), &root, &no_env);
        (out, fake.requests())
    };
    let (out, reqs) = run("deny", 0.95, &["deny", "ask"], "PermissionRequest", "claude-code");
    let v: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(v["hookSpecificOutput"]["decision"]["behavior"], "deny");
    assert!(v["hookSpecificOutput"]["decision"]["message"].as_str().unwrap().contains("deny at P=0.95"));
    assert_eq!(reqs[0].1["state"], json!({"tool": "Bash", "input": {"command": "rm -rf build"}, "cwd": "/p"}));
    // allow isn't in `may`: no decision
    let (out, _) = run("allow", 0.99, &["deny", "ask"], "PermissionRequest", "claude-code");
    assert!(out.stdout.is_empty() && out.note.contains("not allowed"), "{out:?}");
    // allowed, and sure enough
    let (out, _) = run("allow", 0.99, &["allow", "deny", "ask"], "PermissionRequest", "claude-code");
    assert!(out.stdout.contains("\"behavior\":\"allow\""));
    // not sure enough (deny needs 0.8)
    let (out, _) = run("deny", 0.7, &["deny", "ask"], "PermissionRequest", "claude-code");
    assert!(out.stdout.is_empty());
    // the user is asked on a permission request anyway
    let (out, _) = run("ask", 0.9, &["deny", "ask"], "PermissionRequest", "claude-code");
    assert!(out.stdout.is_empty() && out.note.contains("asked anyway"));
    // PreToolUse can ask in Claude Code, not in Codex
    let (out, _) = run("ask", 0.9, &["deny", "ask"], "PreToolUse", "claude-code");
    assert!(out.stdout.contains("\"permissionDecision\":\"ask\""));
    let (out, _) = run("ask", 0.9, &["deny", "ask"], "PreToolUse", "codex");
    assert!(out.stdout.is_empty());
}

#[test]
fn hooks_defer_when_the_decider_is_unavailable() {
    let root = tmp("hook-down");
    let lock = hook_lock("http://127.0.0.1:9", "approve", &["deny"], Value::Null);
    let out = decide::hook("approve", "claude-code", &json!({"hook_event_name": "PermissionRequest"}), &lock, &root, &no_env);
    assert!(out.stdout.is_empty() && out.exit == 0 && out.note.starts_with("decider unavailable"), "{out:?}");
    let out = decide::hook("stop", "claude-code", &json!({"hook_event_name": "Stop"}), &lock, &root, &no_env);
    assert!(out.stdout.is_empty() && out.note.contains("no stop decider"));
}

#[test]
fn stop_hooks_keep_the_agent_going_when_it_isnt_done() {
    let root = tmp("stop");
    write(&root, "t.jsonl", "{\"type\":\"summary\"}\n{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"text\",\"text\":\"Fix the failing test\"}]}}\n");
    let fake = Fake::start(|_, body, _| (200, systemone(body, if body["state"]["last_message"] == "Done!" { 0.95 } else { 0.3 }, 0, "", 0.0)));
    let lock = hook_lock(&fake.url, "stop", &[], Value::Null);
    let ev = |last: &str, active: bool| json!({"hook_event_name": "Stop", "stop_hook_active": active, "last_assistant_message": last, "transcript_path": root.join("t.jsonl")});
    let out = decide::hook("stop", "claude-code", &ev("I looked at it.", false), &lock, &root, &no_env);
    let v: Value = serde_json::from_str(&out.stdout).unwrap();
    assert_eq!(v["decision"], "block");
    assert_eq!(fake.requests()[0].1["state"]["request"], "Fix the failing test");
    assert!(decide::hook("stop", "claude-code", &ev("Done!", false), &lock, &root, &no_env).stdout.is_empty());
    // already continuing because of a stop hook: never loop
    let n = fake.requests().len();
    assert!(decide::hook("stop", "claude-code", &ev("I looked at it.", true), &lock, &root, &no_env).stdout.is_empty());
    assert_eq!(fake.requests().len(), n, "it doesn't even ask");
}

#[test]
fn hook_entries_merge_and_strip_without_touching_the_users_hooks() {
    let mut lock = hook_lock("http://x", "approve", &["deny"], Value::Null);
    lock.decider.push(Decider { use_: "stop".into(), via: "hook".into(), wiring: vec![Wiring { file: "settings".into(), event: "Stop".into() }], ..lock.decider[0].clone() });
    let entries = decide::hook_entries(&lock, "settings", "claude-code", "/usr/local/bin/hlock", Path::new("/p/harness.lock"));
    assert_eq!(entries["PermissionRequest"][0]["matcher"], "*");
    assert_eq!(entries["PermissionRequest"][0]["hooks"][0]["command"], "/usr/local/bin/hlock decide hook approve --format claude-code --lock /p/harness.lock");
    assert!(entries["Stop"][0].get("matcher").is_none());
    let mine = json!({"type": "command", "command": "./my-lint.sh"});
    let mut doc = json!({"model": "opus", "hooks": {"PermissionRequest": [{"matcher": "Bash", "hooks": [mine]}]}});
    decide::merge_hooks(&mut doc, "/hooks", &entries);
    decide::merge_hooks(&mut doc, "/hooks", &entries); // idempotent
    assert_eq!(doc["hooks"]["PermissionRequest"].as_array().unwrap().len(), 2);
    assert_eq!(doc["hooks"]["Stop"].as_array().unwrap().len(), 1);
    assert!(decide::strip_hooks(&mut doc));
    assert_eq!(doc, json!({"model": "opus", "hooks": {"PermissionRequest": [{"matcher": "Bash", "hooks": [mine]}]}}));
    assert!(!decide::strip_hooks(&mut doc));
    // a path with a space is quoted
    assert_eq!(decide::hook_command("hlock", "stop", "codex", Path::new("/my proj/harness.lock")), "hlock decide hook stop --format codex --lock '/my proj/harness.lock'");
}
