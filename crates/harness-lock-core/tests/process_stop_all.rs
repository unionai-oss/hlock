//! `process::stop_all` ends every child any thread is waiting on, so it gets a test
//! binary of its own: in the library's unit tests it would kill other tests' children.

#![cfg(unix)]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use harness_lock_core::process::{run, stop_all, Limits};

#[test]
fn stop_all_ends_running_children() {
    let h = std::thread::spawn(|| {
        let mut c = Command::new("sh");
        c.args(["-c", "sleep 30"]).stdin(Stdio::null());
        run(c, &Limits::default()).unwrap()
    });
    std::thread::sleep(Duration::from_millis(300));
    let t = Instant::now();
    stop_all();
    let out = h.join().unwrap();
    assert!(!out.success && t.elapsed() < Duration::from_secs(5));
}
