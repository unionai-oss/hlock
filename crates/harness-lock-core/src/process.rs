//! Running a child process with a deadline and a cap on what it prints.
//!
//! Every subprocess hlock waits on goes through [`run`]: graders' test commands, the
//! meta agent's `run` tool, code harnesses, secret commands, profile probes, installers
//! and the worker. It drains stdout and stderr while it waits (a child that prints more
//! than a pipe holds can't block), runs the child in its own process group on Unix and
//! stops the whole group on timeout, keeps only the last `max_bytes` of each stream, and
//! never waits on a pipe that a grandchild still holds open.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::{mpsc, Mutex};
use std::time::{Duration, Instant};

/// How long to keep draining the pipes after the child is gone. A grandchild that
/// left the process group (`setsid`, a daemon) can hold them open indefinitely.
const DRAIN_GRACE: Duration = Duration::from_secs(2);

/// The default cap on each captured stream.
pub const MAX_OUTPUT: usize = 1 << 20;

#[derive(Debug, Clone)]
pub struct Limits {
    /// Stop the child (and its group) after this long; `None` waits forever.
    pub timeout: Option<Duration>,
    /// The most of each stream kept (the end of it).
    pub max_bytes: usize,
    /// Also stop what the child left running when it exits normally (a test command's
    /// `server &`). Off for installers, which may start a daemon on purpose.
    pub kill_leftovers: bool,
    /// Left out of [`stop_all`]: cleanup that has to finish even after a Ctrl-C (deleting a
    /// sandbox, and the providers that hold keys).
    pub untracked: bool,
}

impl Default for Limits {
    fn default() -> Self {
        Limits { timeout: None, max_bytes: MAX_OUTPUT, kill_leftovers: true, untracked: false }
    }
}

impl Limits {
    pub fn timeout(t: Duration) -> Self {
        Limits { timeout: Some(t), ..Default::default() }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Output {
    /// The exit code; `None` when the child timed out or was ended by a signal.
    pub code: Option<i32>,
    pub success: bool,
    pub timed_out: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// Whether either stream was longer than `max_bytes` and lost its start.
    pub truncated: bool,
}

impl Output {
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }
    /// stdout followed by stderr, as text.
    pub fn text(&self) -> String {
        format!("{}{}", self.stdout_text(), self.stderr_text())
    }
}

/// Spawn `cmd` (stdout and stderr are piped; stdin is left as the caller set it, or
/// null) and wait for it within `limits`. Errors only when it can't be spawned.
pub fn run(mut cmd: Command, limits: &Limits) -> std::io::Result<Output> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
    let mut child = cmd.spawn()?;
    let _live = (!limits.untracked).then(|| Live::register(&child));
    let max = limits.max_bytes;
    let (tx_out, rx_out) = mpsc::channel();
    let (tx_err, rx_err) = mpsc::channel();
    if let Some(so) = child.stdout.take() {
        std::thread::spawn(move || tx_out.send(read_tail(so, max)));
    }
    if let Some(se) = child.stderr.take() {
        std::thread::spawn(move || tx_err.send(read_tail(se, max)));
    }
    let start = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break Some(s),
            Ok(None) if limits.timeout.is_some_and(|t| start.elapsed() > t) => {
                timed_out = true;
                stop(&mut child);
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => {
                stop(&mut child);
                break None;
            }
        }
    };
    // The group leader is reaped now, but its group id can't be reused while any member
    // lives, so stopping the group is still safe.
    if status.is_some() && limits.kill_leftovers {
        kill_group(&child);
    }
    let deadline = Instant::now() + DRAIN_GRACE;
    let take = |rx: mpsc::Receiver<(Vec<u8>, bool)>| rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap_or_default();
    let (stdout, t1) = take(rx_out);
    let (stderr, t2) = take(rx_err);
    let code = status.and_then(|s| s.code());
    Ok(Output { code, success: status.is_some_and(|s| s.success()), timed_out, stdout, stderr, truncated: t1 || t2 })
}

/// Process groups started by [`run`] that haven't finished, so [`stop_all`] can end them.
static LIVE: Mutex<Vec<u32>> = Mutex::new(Vec::new());

struct Live(u32);

impl Live {
    fn register(child: &Child) -> Live {
        if let Ok(mut v) = LIVE.lock() {
            v.push(child.id());
        }
        Live(child.id())
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        if let Ok(mut v) = LIVE.lock() {
            v.retain(|p| *p != self.0);
        }
    }
}

/// Stop every child (and its group) that [`run`] is waiting on, in any thread. For
/// Ctrl-C: children run in their own process group, so the terminal's SIGINT doesn't
/// reach them. Each `run` then returns as if the child had been killed.
pub fn stop_all() {
    let pids = LIVE.lock().map(|v| v.clone()).unwrap_or_default();
    for pid in pids {
        kill_pid_group(pid);
    }
}

/// Stop the child and everything in its process group, then reap it.
fn stop(child: &mut Child) {
    kill_group(child);
    let _ = child.kill();
    let _ = child.wait();
}

fn kill_group(child: &Child) {
    kill_pid_group(child.id());
}

#[cfg(unix)]
fn kill_pid_group(pid: u32) {
    let Ok(pgid) = libc::pid_t::try_from(pid) else { return };
    if pgid > 0 {
        // SAFETY: killpg(2) takes plain integers and touches no memory of ours. The child
        // was spawned with `process_group(0)`, so its pid is its group id. `run` calls
        // this before reaping the leader (an unreaped zombie keeps the id reserved), or
        // after reaping it while it's still registered in LIVE for `stop_all`; in that
        // short window a reused id would have to land on a new group leader too.
        unsafe {
            libc::killpg(pgid, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn kill_pid_group(_pid: u32) {}

/// Read a stream to its end, keeping only the last `max` bytes. Returns the bytes and
/// whether anything was dropped.
fn read_tail(mut r: impl Read, max: usize) -> (Vec<u8>, bool) {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut dropped = false;
    loop {
        match r.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                if buf.len() > max.saturating_mul(2).max(8192) {
                    let cut = buf.len() - max;
                    buf.drain(..cut);
                    dropped = true;
                }
            }
        }
    }
    if buf.len() > max {
        let cut = buf.len() - max;
        buf.drain(..cut);
        dropped = true;
    }
    (buf, dropped)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn sh(line: &str) -> Command {
        let mut c = Command::new("sh");
        c.args(["-c", line]).stdin(Stdio::null());
        c
    }

    #[test]
    fn captures_and_reports_the_exit_code() {
        let out = run(sh("echo hi; echo err >&2; exit 3"), &Limits::default()).unwrap();
        assert_eq!((out.code, out.success, out.timed_out), (Some(3), false, false));
        assert_eq!((out.stdout_text().as_str(), out.stderr_text().as_str()), ("hi\n", "err\n"));
    }

    #[test]
    fn output_larger_than_a_pipe_does_not_block() {
        let out = run(sh("head -c 300000 /dev/zero"), &Limits::timeout(Duration::from_secs(10))).unwrap();
        assert!(out.success && !out.timed_out);
        assert_eq!(out.stdout.len(), 300_000);
    }

    #[test]
    fn keeps_only_the_tail() {
        let out = run(sh("yes | head -c 100000; echo END"), &Limits { max_bytes: 100, ..Default::default() }).unwrap();
        assert!(out.truncated && out.stdout.len() == 100 && out.stdout_text().ends_with("END\n"));
    }

    #[test]
    fn a_timeout_stops_the_whole_group() {
        let t = Instant::now();
        let out = run(sh("sleep 30 & sleep 30"), &Limits::timeout(Duration::from_millis(300))).unwrap();
        assert!(out.timed_out && out.code.is_none());
        assert!(t.elapsed() < Duration::from_secs(5), "took {:?}", t.elapsed());
    }

    #[test]
    fn leftovers_holding_the_pipe_do_not_hang_a_normal_exit() {
        let t = Instant::now();
        let out = run(sh("sleep 30 & echo hi"), &Limits::timeout(Duration::from_secs(10))).unwrap();
        assert!(out.success && out.stdout_text() == "hi\n");
        assert!(t.elapsed() < Duration::from_secs(5), "took {:?}", t.elapsed());
        // without killing leftovers, the drain still gives up after the grace period
        let t = Instant::now();
        let out = run(sh("sleep 30 & echo hi"), &Limits { kill_leftovers: false, ..Limits::timeout(Duration::from_secs(10)) }).unwrap();
        assert!(out.success);
        assert!(t.elapsed() < DRAIN_GRACE + Duration::from_secs(2), "took {:?}", t.elapsed());
    }
}
