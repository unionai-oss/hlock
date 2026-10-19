//! Graders: artifact content checks, tests commands and scripts, judges, and
//! `EvalPack::check_graders` against a task's solution.

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};

use harness_lock_core::tasks::{EvalPack, GradeOptions, JudgeConfig, Judges};

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hlock-graders-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn write(root: &Path, rel: &str, text: &str) {
    let p = root.join(rel);
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, text).unwrap();
}

/// A one-task pack whose task has the given extra `[[task]]` lines.
fn pack(name: &str, task: &str) -> (PathBuf, EvalPack) {
    let root = tmp(name);
    write(&root, "pack.toml", &format!(
        "[pack]\nname = \"t/{name}\"\nversion = \"0.1.0\"\ndomain = \"test\"\n\n[[task]]\nid = \"a\"\nprompt = \"a/prompt.md\"\n{task}\n"
    ));
    write(&root, "a/prompt.md", "Count the words in words.txt and write the number to count.txt.\n");
    let p = EvalPack::load(root.to_str().unwrap(), None).unwrap();
    (root, p)
}

fn grade(p: &EvalPack, ws: &Path) -> Vec<(String, bool, f64, String)> {
    p.grade(&p.tasks[0], ws).into_iter().map(|g| (g.grader, g.success, g.score, g.detail)).collect()
}

#[test]
fn artifact_content_checks() {
    let (root, p) = pack("artifact", r#"
expected = { artifacts = ["count.txt", "notes.md"] }
graders = [
  { type = "artifact", path = "count.txt", equals = "9" },
  { type = "artifact", path = "notes.md", contains = ["fox", "dog"] },
]"#);
    let ws = root.join("ws");
    write(&ws, "count.txt", "  9\n");
    write(&ws, "notes.md", "the fox and the dog\n");
    let g = grade(&p, &ws);
    // the expected artifacts are covered by the graders, so there's no separate existence check
    assert_eq!(g.len(), 2, "{g:?}");
    assert!(g.iter().all(|g| g.1), "{g:?}");

    write(&ws, "count.txt", "10\n");
    write(&ws, "notes.md", "a fox\n");
    let g = grade(&p, &ws);
    assert_eq!(g[0].3, r#"count.txt should be "9", but is "10""#);
    assert_eq!(g[1].3, r#"notes.md doesn't contain "dog""#);
    assert!(g.iter().all(|g| !g.1 && g.2 == 0.0));

    fs::remove_file(ws.join("notes.md")).unwrap();
    assert_eq!(grade(&p, &ws)[1].3, "notes.md is missing");
}

#[test]
fn same_as_compares_with_a_golden_file_and_existence_still_counts() {
    let (root, p) = pack("golden", r#"
expected = { artifacts = ["out.csv", "log.txt"] }
graders = [{ type = "artifact", path = "out.csv", same-as = "a/golden.csv" }]"#);
    write(&root, "a/golden.csv", "id,total\n1,10\n");
    let ws = root.join("ws");
    write(&ws, "out.csv", "id,total\r\n1,10\r\n");
    let g = grade(&p, &ws);
    // log.txt isn't covered by a grader, so it gets the plain existence check
    assert_eq!(g.iter().map(|g| (g.0.as_str(), g.1)).collect::<Vec<_>>(), [("artifact:log.txt", false), ("artifact:out.csv", true)]);
}

#[cfg(unix)]
#[test]
fn tests_graders_run_commands_and_scripts() {
    let (root, p) = pack("tests", r#"
fixtures = "a/workspace/"
graders = [
  { type = "tests", command = "sh hidden/check.sh", files = ["a/hidden"] },
  "tests:a/score.py",
]"#);
    write(&root, "a/hidden/check.sh", "test \"$(cat count.txt)\" = 9 || { echo \"count.txt is $(cat count.txt), not 9\"; exit 1; }\n");
    // a script reports partial credit through $HLOCK_GRADE
    write(&root, "a/score.py", r#"import json, os
words = open("count.txt").read().split()
json.dump({"score": 0.5, "success": False, "detail": f"{len(words)} words in {os.environ['HLOCK_TASK']}"}, open(os.environ["HLOCK_GRADE"], "w"))
"#);
    let ws = root.join("ws");
    write(&ws, "count.txt", "9\n");
    let g = grade(&p, &ws);
    assert_eq!((g[0].0.as_str(), g[0].1, g[0].2), ("tests:sh hidden/check.sh", true, 1.0), "{g:?}");
    assert!(ws.join("hidden/check.sh").is_file(), "files are copied in before the command runs");
    assert_eq!((g[1].0.as_str(), g[1].1, g[1].2, g[1].3.as_str()), ("tests:a/score.py", false, 0.5, "1 words in a"));

    write(&ws, "count.txt", "8\n");
    let g = grade(&p, &ws);
    assert!(!g[0].1 && g[0].3.starts_with("exit code 1\ncount.txt is 8, not 9"), "{g:?}");
}

#[cfg(unix)]
#[test]
fn a_slow_test_command_times_out() {
    let (root, p) = pack("timeout", r#"graders = [{ type = "tests", command = "sleep 5", timeout-seconds = 1 }]"#);
    let g = grade(&p, &root);
    assert!(!g[0].1 && g[0].3.contains("timed out after 1s"), "{g:?}");
}

#[test]
fn check_reports_bad_graders() {
    let (root, p) = pack("bad", r#"
expected = { artifacts = ["out.md"] }
graders = [
  "lint:x",
  { type = "tests", command = "true", script = "x.py" },
  { type = "artifact", path = "out.md", equal = "x" },
  "judge:a/rubric.md",
  "tests:a/missing.py",
  "trace:no-rm-rf",
]"#);
    write(&root, "a/rubric.md", "- TODO(hlock): what a good answer has\n");
    let p = EvalPack::load(p.path.to_str().unwrap(), None).unwrap();
    let issues: Vec<(String, String)> = p.check().into_iter().map(|i| (i.level, i.message)).collect();
    let has = |level: &str, m: &str| issues.iter().any(|(l, x)| l == level && x == m);
    assert!(has("error", r#"grader 1: unknown grader type "lint" (use artifact, tests, judge, or trace)"#), "{issues:?}");
    assert!(has("error", "grader 2: a tests grader needs a `command` or a `script` (one of them)"), "{issues:?}");
    assert!(has("error", "grader 3: unknown key `equal` in a artifact grader (expected path, paths, equals, same-as, contains)"), "{issues:?}");
    assert!(has("error", "judge grader file a/rubric.md still has 1 TODO(hlock)"), "{issues:?}");
    assert!(has("error", "tests grader file a/missing.py not found"), "{issues:?}");
    assert!(issues.iter().any(|(l, m)| l == "warn" && m.starts_with("judge graders need a model")), "{issues:?}");
    // a judge from harness.toml satisfies it
    let judge = JudgeConfig { model: "openai:gpt-x".into(), ..Default::default() };
    assert!(!p.check_with(Some(&judge)).iter().any(|i| i.message.starts_with("judge graders need")));
}

#[test]
fn existence_only_tasks_get_a_nudge() {
    let (_, p) = pack("nudge", r#"expected = { artifacts = ["out.md"] }"#);
    let warns: Vec<String> = p.check().into_iter().filter(|i| i.level == "warn").map(|i| i.message).collect();
    assert!(warns.contains(&"only checks that out.md exists; add a content check, tests, or a judge".to_string()), "{warns:?}");
}

#[cfg(unix)]
#[test]
fn check_graders_tries_the_fixtures_and_the_solution() {
    let root = tmp("solutions");
    write(&root, "pack.toml", r#"
[pack]
name = "t/solutions"
version = "0.1.0"
domain = "test"

[[task]]
id = "good"
prompt = "p.md"
fixtures = "good/workspace/"
solution = "good/solution/"
graders = [{ type = "artifact", path = "count.txt", equals = "9" }]

[[task]]
id = "wrong_solution"
prompt = "p.md"
solution = "wrong/solution/"
graders = [{ type = "tests", command = "grep -q 9 count.txt" }]

[[task]]
id = "free"
prompt = "p.md"
fixtures = "free/workspace/"
graders = [{ type = "artifact", path = "count.txt", contains = ["9"] }]
"#);
    write(&root, "p.md", "Write 9 to count.txt.\n");
    write(&root, "good/workspace/words.txt", "one two\n");
    write(&root, "good/solution/count.txt", "9\n");
    write(&root, "wrong/solution/count.txt", "8\n");
    write(&root, "free/workspace/count.txt", "9\n");
    let p = EvalPack::load(root.to_str().unwrap(), None).unwrap();
    let issues: Vec<(String, Option<String>, String)> = p.check_graders(&GradeOptions { judges: Judges::None, ..Default::default() }).into_iter().map(|i| (i.level, i.task, i.message)).collect();
    assert_eq!(issues, [
        ("error".to_string(), Some("wrong_solution".to_string()), "its solution fails tests:grep -q 9 count.txt: exit code 1".to_string()),
        ("warn".into(), Some("free".into()), "its graders pass on the fixtures alone, so it passes without any work".into()),
        ("warn".into(), Some("free".into()), "no solution, so its graders weren't tried on a known-good result".into()),
    ]);
}

/// Serve one OpenAI-style chat completion whose content is `reply`; returns the base
/// URL and a handle yielding the request body.
fn fake_model(reply: &str) -> (String, std::thread::JoinHandle<String>) {
    serve(serde_json::json!({"choices": [{"message": {"content": reply}}], "usage": {"prompt_tokens": 10, "completion_tokens": 5}}))
}

/// Serve one HTTP response with `body` as JSON.
fn serve(body: serde_json::Value) -> (String, std::thread::JoinHandle<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let body = body.to_string();
    let h = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut r = BufReader::new(s.try_clone().unwrap());
        let mut len = 0;
        loop {
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                len = v.trim().parse().unwrap();
            }
            if line == "\r\n" {
                break;
            }
        }
        let mut req = vec![0; len];
        r.read_exact(&mut req).unwrap();
        write!(s, "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).unwrap();
        String::from_utf8(req).unwrap()
    });
    (url, h)
}

#[test]
fn judges_score_files_against_criteria() {
    let (root, p) = pack("judge", r#"
expected = { artifacts = ["summary.md"] }
graders = [{ type = "judge", criteria = ["Names the revenue figure", "Under 100 words", "Cites the source"], pass = 0.6 }]"#);
    let ws = root.join("ws");
    write(&ws, "summary.md", "Revenue was $1.2M.\n");
    let (url, req) = fake_model("Here you go:\n{\"criteria\": [true, true, false], \"reason\": \"No source is cited.\"}");
    let judge = JudgeConfig { model: "local:tiny".into(), base_url: Some(url), key_env: None, ..Default::default() };
    let g = p.grade_with(&p.tasks[0], &ws, &GradeOptions { judge: Some(judge), ..Default::default() });
    let sent: serde_json::Value = serde_json::from_str(&req.join().unwrap()).unwrap();
    assert_eq!(sent["model"], "tiny");
    let prompt = sent["messages"][1]["content"].as_str().unwrap();
    assert!(prompt.contains("<criteria>\n1. Names the revenue figure\n2. Under 100 words\n3. Cites the source\n</criteria>"), "{prompt}");
    assert!(prompt.contains("<file path=\"summary.md\">\nRevenue was $1.2M.\n\n</file>"), "{prompt}");
    // summary.md must exist, and the judge scores it
    assert_eq!(g.iter().map(|g| g.grader.as_str()).collect::<Vec<_>>(), ["artifact:summary.md", "judge:3 criteria (local:tiny)"]);
    assert!(g[1].success && (g[1].score - 2.0 / 3.0).abs() < 1e-9, "{g:?}");
    assert_eq!(g[1].detail, "unmet: 3. No source is cited.");
}

#[test]
fn judges_need_a_model_and_are_skipped_offline() {
    let (root, p) = pack("nojudge", r#"
expected = { artifacts = ["summary.md"] }
graders = ["judge:a/rubric.md"]"#);
    write(&root, "a/rubric.md", "A good summary names the revenue figure.\n");
    let g = grade(&p, &root);
    assert!(!g[1].1 && g[1].3.starts_with("no judge model"), "{g:?}");
    let offline = p.grade_with(&p.tasks[0], &root, &GradeOptions { judges: Judges::None, ..Default::default() });
    assert_eq!(offline.iter().map(|g| g.grader.as_str()).collect::<Vec<_>>(), ["artifact:summary.md"]);
}

#[test]
fn judges_on_anthropic_use_the_messages_api() {
    let (root, p) = pack("anthropic", r#"
expected = { artifacts = ["summary.md"] }
graders = ["judge:a/rubric.md"]"#);
    write(&root, "a/rubric.md", "A good summary names the revenue figure.\n");
    let ws = root.join("ws");
    write(&ws, "summary.md", "Revenue was $1.2M.\n");
    let (url, req) = serve(serde_json::json!({"content": [{"type": "text", "text": "{\"score\": 0.4, \"reason\": \"Vague.\"}"}], "usage": {"input_tokens": 9, "output_tokens": 4}}));
    let judge = JudgeConfig { model: "anthropic:claude-x".into(), base_url: Some(url), key_env: Some("HLOCK_TEST_NO_SUCH_KEY".into()), ..Default::default() };
    let g = p.grade_with(&p.tasks[0], &ws, &GradeOptions { judge: Some(judge), ..Default::default() });
    let sent: serde_json::Value = serde_json::from_str(&req.join().unwrap()).unwrap();
    assert_eq!(sent["model"], "claude-x");
    assert!(sent["system"].as_str().unwrap().contains("Reply with JSON only"));
    assert!(sent["messages"][0]["content"].as_str().unwrap().contains("<rubric>\nA good summary names the revenue figure.\n</rubric>"));
    assert_eq!((g[1].grader.as_str(), g[1].success, g[1].score, g[1].detail.as_str()), ("judge:a/rubric.md (anthropic:claude-x)", false, 0.4, "Vague."));
}

#[test]
fn grader_entries_that_dont_parse_say_why() {
    let (_, p) = pack("parse", r#"
expected = { artifacts = ["out.md"] }
graders = [
  "tests:",
  "judge",
  42,
  { path = "out.md" },
  { type = "artifact", path = "out.md", equals = "x", same-as = "y" },
  { type = "artifact", path = 3 },
  { type = "tests", script = "c.py", timeout-seconds = 0 },
  { type = "judge", rubric = "r.md", criteria = ["a"] },
  { type = "judge", criteria = ["a"], pass = 1.5 },
  { type = "judge", criteria = [1, 2] },
  "artifact:a.txt, b.txt",
  "artifact",
]"#);
    let got: Vec<Result<String, String>> = p.tasks[0].grader_list().into_iter().map(|g| g.map(|g| g.label())).collect();
    let errs: Vec<String> = got.iter().filter_map(|g| g.clone().err()).collect();
    assert_eq!(errs, [
        r#""tests:" needs a file: "tests:<path in the pack>""#,
        r#""judge" needs a file: "judge:<path in the pack>""#,
        r#"a grader must be a string like "tests:check.py" or a table with a `type`"#,
        "a grader table needs a `type` (artifact, tests, judge, or trace)",
        "set `equals` or `same-as`, not both",
        "`path` must be a string",
        "`timeout-seconds` must be a positive integer",
        "a judge grader needs a `rubric` file or inline `criteria` and `levels`, not both",
        "`pass` must be a number from 0 to 1",
        "`criteria` must be a list of strings or tables",
    ]);
    // shorthand artifact graders take a comma list, or default to expected.artifacts
    assert_eq!(got[10], Ok("artifact:a.txt,b.txt".into()));
    assert_eq!(got[11], Ok("artifact:out.md".into()));
    // grading reports a bad entry as a failed grade rather than skipping it
    let g = p.grade(&p.tasks[0], &p.path);
    assert!(g.iter().any(|g| g.grader == "invalid" && !g.success), "{g:?}");
}

#[test]
fn judge_replies_are_read_strictly() {
    use harness_lock_core::graders::judge_reply;
    assert_eq!(judge_reply("```json\n{\"score\": 0.8, \"reason\": \"ok\"}\n```", 0).unwrap(), (0.8, "ok".to_string()));
    assert_eq!(judge_reply("{\"score\": 3}", 0).unwrap().0, 1.0, "scores are clamped");
    assert_eq!(judge_reply("{\"criteria\": [{\"met\": true}, false], \"reason\": \"half\"}", 2).unwrap(), (0.5, "unmet: 2. half".to_string()));
    assert_eq!(judge_reply("{\"criteria\": [true]}", 2).unwrap_err(), "the judge answered 1 of 2 criteria");
    assert!(judge_reply("I think it's fine", 0).unwrap_err().starts_with("the judge didn't reply with JSON"));
    assert!(judge_reply("{\"reason\": \"no score\"}", 0).is_err());
}

#[test]
fn a_judge_without_its_key_fails_with_the_variable_to_set() {
    let (root, p) = pack("nokey", r#"
expected = { artifacts = ["summary.md"] }
graders = [{ type = "judge", criteria = ["Is short"] }]"#);
    write(&root, "summary.md", "short\n");
    let judge = JudgeConfig { model: "openai:gpt-x".into(), key_env: Some("HLOCK_TEST_UNSET_KEY".into()), base_url: None, ..Default::default() };
    let g = p.grade_with(&p.tasks[0], &root, &GradeOptions { judge: Some(judge), ..Default::default() });
    assert_eq!(g[1].detail, "HLOCK_TEST_UNSET_KEY isn't set; the judge reads its openai API key from it");
    let judge = JudgeConfig { model: "no-colon".into(), ..Default::default() };
    let g = p.grade_with(&p.tasks[0], &root, &GradeOptions { judge: Some(judge), ..Default::default() });
    assert_eq!(g[1].detail, r#"judge model "no-colon" isn't written provider:model"#);
}

#[cfg(unix)]
#[test]
fn tests_graders_get_their_environment_and_hidden_files_win() {
    let (root, p) = pack("env", r#"
fixtures = "a/workspace/"
graders = [
  { type = "tests", script = "a/check.sh", files = ["a/expected.txt"] },
  { type = "tests", command = "exit 3" },
  { type = "tests", command = "echo '{\"success\": true, \"score\": 0.25}' > \"$HLOCK_GRADE\"; exit 1" },
]"#);
    write(&root, "a/check.sh", "#!/bin/sh\necho \"$HLOCK_TASK $(basename \"$HLOCK_PACK_DIR\") $(pwd -P | xargs basename)\" > env.txt\ncmp -s expected.txt answer.txt\n");
    let mut perms = fs::metadata(root.join("a/check.sh")).unwrap().permissions();
    std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);
    fs::set_permissions(root.join("a/check.sh"), perms).unwrap();
    write(&root, "a/expected.txt", "42\n");
    let ws = root.join("ws");
    write(&ws, "answer.txt", "42\n");
    // the agent wrote its own expected.txt; the pack's copy replaces it before the check runs
    write(&ws, "expected.txt", "anything\n");
    let g = grade(&p, &ws);
    assert!(g[0].1, "{g:?}");
    let pack_name = root.file_name().unwrap().to_str().unwrap().to_string();
    assert_eq!(fs::read_to_string(ws.join("env.txt")).unwrap().trim(), format!("a {pack_name} ws"));
    assert_eq!((g[1].1, g[1].2, g[1].3.as_str()), (false, 0.0, "exit code 3"));
    // a reported result overrides the exit code
    assert_eq!((g[2].1, g[2].2), (true, 0.25), "{g:?}");
}

#[cfg(unix)]
#[test]
fn a_timeout_stops_what_the_command_started() {
    // the shell forks `sleep` (it isn't the last command), which holds the output pipe open
    let (root, p) = pack("grandchild", r#"graders = [{ type = "tests", command = "sleep 30; echo done", timeout-seconds = 1 }]"#);
    let start = std::time::Instant::now();
    let g = grade(&p, &root);
    assert!(start.elapsed() < std::time::Duration::from_secs(10), "took {:?}", start.elapsed());
    assert!(!g[0].1 && g[0].3.contains("timed out after 1s"), "{g:?}");
}

#[test]
fn check_names_the_files_a_task_only_checks_exist() {
    let (_, p) = pack("nudge2", r#"graders = ["artifact:a.txt"]"#);
    let warns: Vec<String> = p.check().into_iter().filter(|i| i.level == "warn").map(|i| i.message).collect();
    assert!(warns.contains(&"only checks that a.txt exists; add a content check, tests, or a judge".to_string()), "{warns:?}");
    // a trace grader checks the run, so there's no nudge
    let (_, p) = pack("nudge3", r#"graders = ["artifact:a.txt", "trace:Never ran sudo"]"#);
    assert!(!p.check().iter().any(|i| i.message.starts_with("only checks")));
}

#[test]
fn check_graders_doesnt_claim_a_judged_task_passes_with_no_work() {
    let (root, p) = pack("judged", r#"
fixtures = "a/workspace/"
expected = { artifacts = ["summary.md"] }
graders = [{ type = "judge", criteria = ["Is short"] }]"#);
    write(&root, "a/workspace/summary.md", "already here\n");
    let p = EvalPack::load(p.path.to_str().unwrap(), None).unwrap();
    let issues: Vec<String> = p.check_graders(&GradeOptions { judges: Judges::None, ..Default::default() }).into_iter().map(|i| i.message).collect();
    assert!(!issues.iter().any(|m| m.contains("without any work")), "{issues:?}");
}

#[test]
fn grade_dir_grades_a_copy_or_the_solution() {
    let (root, p) = pack("dir", r#"
fixtures = "a/workspace/"
solution = "a/solution/"
graders = [{ type = "artifact", path = "count.txt", equals = "9" }]"#);
    write(&root, "a/workspace/words.txt", "w\n");
    write(&root, "a/solution/count.txt", "9\n");
    let p = EvalPack::load(p.path.to_str().unwrap(), None).unwrap();
    let opts = GradeOptions::default();
    assert!(p.grade_dir("a", None, &opts).unwrap().iter().all(|g| g.success));
    let run = root.join("run");
    write(&run, "count.txt", "8\n");
    let g = p.grade_dir("a", Some(&run), &opts).unwrap();
    assert!(!g[0].success);
    assert_eq!(fs::read_dir(&run).unwrap().count(), 1, "the directory is left as it was");
    assert_eq!(p.grade_dir("nope", None, &opts).unwrap_err().to_string().contains("no task \"nope\""), true);
    assert!(p.grade_dir("a", Some(&root.join("missing")), &opts).unwrap_err().to_string().contains("isn't a directory"));
    let (_, nosol) = pack("dir2", r#"graders = ["artifact:x"]"#);
    assert!(nosol.grade_dir("a", None, &opts).unwrap_err().to_string().contains("has no solution directory"));
}

#[test]
fn the_manifest_judge_overrides_the_packs() {
    let m = harness_lock_core::Manifest::from_str("[evals]\npacks = [\"./evals\"]\n[evals.judge]\nmodel = \"openai:gpt-x\"\nkey-env = \"K\"\n").unwrap();
    let j = m.judge().unwrap();
    assert_eq!((j.model.as_str(), j.key_env.as_deref()), ("openai:gpt-x", Some("K")));
    assert!(harness_lock_core::Manifest::from_str("[evals.judge]\nmodel = \"\"\n").unwrap().judge().is_none());

    let (root, p) = pack("override", r#"
expected = { artifacts = ["s.md"] }
graders = [{ type = "judge", criteria = ["Is short"] }]"#);
    let text = fs::read_to_string(root.join("pack.toml")).unwrap().replace("domain = \"test\"\n", "domain = \"test\"\n\n[pack.judge]\nmodel = \"pack:model\"\nbase-url = \"http://127.0.0.1:9\"\n");
    fs::write(root.join("pack.toml"), text).unwrap();
    let p = EvalPack::load(p.path.to_str().unwrap(), None).unwrap();
    assert_eq!(p.judge.as_ref().unwrap().model, "pack:model");
    write(&root, "s.md", "x\n");
    // the pack's judge (unreachable here) is used, unless options name another
    let g = p.grade(&p.tasks[0], &root);
    assert!(g[1].grader.ends_with("(pack:model)") && g[1].detail.contains("cannot reach"), "{g:?}");
    let judge = JudgeConfig { model: "openai:gpt-x".into(), key_env: Some("HLOCK_TEST_UNSET_KEY".into()), base_url: None, ..Default::default() };
    let g = p.grade_with(&p.tasks[0], &root, &GradeOptions { judge: Some(judge), ..Default::default() });
    assert!(g[1].grader.ends_with("(openai:gpt-x)"), "{g:?}");
}
