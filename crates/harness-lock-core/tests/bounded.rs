//! Subprocesses that hang: code harnesses, the Python worker and the built-in agent's
//! commands are stopped at their timeout, with what they left running. Kept apart from
//! the unit tests, where `process::stop_all`'s own test would stop these children too.
#![cfg(unix)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use harness_lock_core::agent::{self, AgentConfig};
use harness_lock_core::contract::{self, Invocation};
use harness_lock_core::profiles::Ctx;
use harness_lock_core::worker::PythonWorker;
use serde_json::{json, Value};

fn tmp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hlock-bounded-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn a_hung_code_harness_is_stopped_with_its_children() {
    let d = tmp("contract");
    let inv = Invocation { argv: ["sh", "-c", "sleep 30 & sleep 30"].map(String::from).to_vec(), env: BTreeMap::new(), cwd: d.clone(), output: d.join("result.json") };
    let t = Instant::now();
    let r = contract::run(&inv, &Ctx::at(&d), Duration::from_millis(300)).unwrap();
    assert!(!r.success && r.exit_code.is_none() && r.log.contains("timed out"), "{r:?}");
    assert!(t.elapsed() < Duration::from_secs(5), "took {:?}", t.elapsed());
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn a_hung_worker_times_out() {
    let d = tmp("worker");
    let script = d.join("fake-python");
    std::fs::write(&script, "#!/bin/sh\nsleep 30\n").unwrap();
    std::fs::set_permissions(&script, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    // read when the worker is made; nothing else in this test binary reads them
    std::env::set_var("HL_PYTHON", &script);
    std::env::set_var("HLOCK_WORKER_TIMEOUT", "1");
    let w = PythonWorker::new();
    std::env::remove_var("HL_PYTHON");
    std::env::remove_var("HLOCK_WORKER_TIMEOUT");
    let t = Instant::now();
    let err = w.call("grade", "devbox", None, None, 0, &d, &json!({})).unwrap_err().to_string();
    assert!(err.contains("timed out after 1s") && err.contains("HLOCK_WORKER_TIMEOUT"), "{err}");
    assert!(t.elapsed() < Duration::from_secs(6), "took {:?}", t.elapsed());
    let _ = std::fs::remove_dir_all(&d);
}

/// A chat-completions endpoint: `answer` gets the request body and its number (from 1).
fn fake_llm(answer: impl Fn(&Value, usize) -> Value + Send + 'static) -> (String, Arc<Mutex<Vec<Value>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
    let log = seen.clone();
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
            let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let n = {
                let mut l = log.lock().unwrap();
                l.push(body.clone());
                l.len()
            };
            let text = answer(&body, n).to_string();
            let _ = write!(s, "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{text}", text.len());
        }
    });
    (url, seen)
}

fn run_call(command: &str) -> Value {
    let args = json!({"command": command}).to_string();
    json!({"choices": [{"message": {"content": "", "tool_calls": [{"id": "c1", "type": "function", "function": {"name": "run", "arguments": args}}]}}], "usage": {}})
}

fn config(url: &str, project: &Path) -> AgentConfig {
    AgentConfig { provider: "local".into(), model: "m".into(), base_url: url.into(), max_steps: 5, timeout_seconds: 1, interactive: true, project: project.into(), ..Default::default() }
}

#[test]
fn the_agents_commands_time_out() {
    let d = tmp("agent");
    let (url, seen) = fake_llm(|_, n| if n == 1 { run_call("sleep 30 & sleep 30") } else { json!({"choices": [{"message": {"content": "done"}}], "usage": {}}) });
    let t = Instant::now();
    let out = agent::run(&config(&url, &d), "go", &mut |_, _| true, &mut |_| "y".into());
    assert_eq!(out.status, "done");
    assert!(t.elapsed() < Duration::from_secs(8), "took {:?}", t.elapsed());
    let second = seen.lock().unwrap()[1].clone();
    let result = second["messages"].as_array().unwrap().last().unwrap()["content"].as_str().unwrap().to_string();
    assert!(result.starts_with("timed out after 1s"), "{result}");
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn stopping_at_a_result_stops_the_run() {
    let d = tmp("stop");
    let (url, seen) = fake_llm(|_, _| run_call("true"));
    let out = agent::run(&config(&url, &d), "go", &mut |kind, _| kind != "result", &mut |_| "y".into());
    assert_eq!((out.status.as_str(), out.steps), ("stopped", 1));
    assert_eq!(seen.lock().unwrap().len(), 1);
    let _ = std::fs::remove_dir_all(&d);
}
