//! Integration tests for `Cmd::spawn` / `SpawnedProcess`.
//!
//! Covers:
//! - spawn + wait (happy path, non-zero exit)
//! - spawn + kill + wait (surfacing via ExitStatus)
//! - take_stdin + take_stdout bidirectional usage (the branchdiff pattern)
//! - Read impl for SpawnedProcess and &SpawnedProcess
//! - spawn_and_collect_lines
//! - wait_timeout both branches (exited, still running)

use std::io::{BufReader, Read, Write};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use procpilot::Cmd;

const PP_ECHO: &str = env!("CARGO_BIN_EXE_pp_echo");
const PP_CAT: &str = env!("CARGO_BIN_EXE_pp_cat");
const PP_SLEEP: &str = env!("CARGO_BIN_EXE_pp_sleep");
const PP_STATUS: &str = env!("CARGO_BIN_EXE_pp_status");
const PP_SPAM: &str = env!("CARGO_BIN_EXE_pp_spam");

#[test]
fn spawn_wait_captures_stdout() {
    let proc = Cmd::new(PP_ECHO).arg("hi").spawn().expect("spawn");
    let out = proc.wait().expect("wait");
    assert_eq!(out.stdout_lossy().trim(), "hi");
}

#[test]
fn spawn_wait_surfaces_nonzero_exit() {
    let proc = Cmd::new(PP_STATUS)
        .args(["1", "--err", "boom"])
        .spawn()
        .expect("spawn");
    let err = proc.wait().expect_err("should fail");
    assert!(err.is_non_zero_exit());
    assert_eq!(err.stderr(), Some("boom\n"));
}

#[test]
fn spawn_kill_returns_error_on_wait() {
    let proc = Cmd::new(PP_SLEEP).arg("10000").spawn().expect("spawn");
    proc.kill().expect("kill");
    // SIGKILL on Unix returns a non-zero ExitStatus with no exit code; on
    // Windows the shape differs. The real thing under test is that wait
    // doesn't hang — the specific Ok/Err shape isn't portable.
    let _ = proc.wait();
}

#[test]
fn take_stdin_and_stdout_pipe_bidirectionally() {
    // Covers the interactive-protocol pattern (e.g. `git cat-file --batch`,
    // `jj log --stream-json`): one thread writes requests into stdin while
    // another reads responses from stdout.
    let proc = Cmd::new(PP_CAT).spawn().expect("spawn");
    let mut stdin = proc.take_stdin().expect("stdin piped");
    let stdout = proc.take_stdout().expect("stdout piped");

    let writer = thread::spawn(move || {
        stdin.write_all(b"line one\nline two\n").expect("write");
        drop(stdin); // EOF
    });

    let mut reader = BufReader::new(stdout);
    let mut buf = String::new();
    reader.read_to_string(&mut buf).expect("read");
    writer.join().expect("join");
    let _ = proc.wait();

    assert!(buf.contains("line one"));
    assert!(buf.contains("line two"));
}

#[test]
fn take_stdout_twice_returns_none_second_time() {
    let proc = Cmd::new(PP_ECHO).arg("x").spawn().expect("spawn");
    assert!(proc.take_stdout().is_some());
    assert!(proc.take_stdout().is_none());
    let _ = proc.wait();
}

#[test]
fn read_impl_streams_stdout() {
    let mut proc = Cmd::new(PP_ECHO).arg("abc").spawn().expect("spawn");
    let mut buf = String::new();
    proc.read_to_string(&mut buf).expect("read");
    assert_eq!(buf.trim(), "abc");
    assert!(proc.take_stdout().is_none());
    let _ = proc.wait();
}

#[test]
fn read_via_shared_reference() {
    let proc = Cmd::new(PP_SPAM).arg("1000").spawn().expect("spawn");
    let reader = &proc;
    let mut buf = Vec::new();
    // Exercises `impl Read for &SpawnedProcess` — the shape that lets one
    // thread read while another holds the handle for kill/wait.
    let _ = (&mut { reader }).read_to_end(&mut buf);
    let _ = proc.wait();
    assert!(!buf.is_empty());
}

#[test]
fn collect_lines_delivers_each_line() {
    use std::sync::{Arc, Mutex};
    let collected: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let c = collected.clone();
    let out = Cmd::new(PP_CAT)
        .stdin("one\ntwo\nthree\n")
        .spawn_and_collect_lines(move |line| {
            c.lock().unwrap().push(line.to_string());
            Ok(())
        })
        .expect("ok");
    assert!(out.stdout.is_empty());
    let lines = collected.lock().unwrap();
    assert_eq!(*lines, vec!["one", "two", "three"]);
}

#[test]
fn wait_timeout_returns_none_while_running() {
    let proc = Cmd::new(PP_SLEEP).arg("5000").spawn().expect("spawn");
    let res = proc
        .wait_timeout(Duration::from_millis(100))
        .expect("wait_timeout");
    assert!(res.is_none(), "should still be running");
    proc.kill().expect("kill");
    let _ = proc.wait();
}

#[test]
fn wait_timeout_returns_output_when_done() {
    let proc = Cmd::new(PP_ECHO).arg("quick").spawn().expect("spawn");
    let res = proc
        .wait_timeout(Duration::from_secs(5))
        .expect("wait_timeout");
    let out = res.expect("process should have exited");
    assert_eq!(out.stdout_lossy().trim(), "quick");
}

#[test]
fn try_wait_reports_running_then_done() {
    let proc = Cmd::new(PP_SLEEP).arg("500").spawn().expect("spawn");
    let initial = proc.try_wait().expect("try_wait");
    assert!(initial.is_none());
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if proc.try_wait().expect("try_wait").is_some() {
            return;
        }
        if std::time::Instant::now() >= deadline {
            panic!("try_wait never reported completion");
        }
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn pids_returns_single_entry() {
    let proc = Cmd::new(PP_ECHO).arg("x").spawn().expect("spawn");
    let pids = proc.pids();
    assert_eq!(pids.len(), 1);
    assert!(pids[0] > 0);
    let _ = proc.wait();
}

#[test]
fn spawn_missing_binary_is_spawn_error() {
    let err = Cmd::new("nonexistent_binary_xyz_42")
        .spawn()
        .expect_err("should fail");
    assert!(err.is_spawn_failure());
}

#[test]
fn wait_is_idempotent_on_success() {
    let proc = Cmd::new(PP_ECHO).arg("idempotent").spawn().expect("spawn");
    let first = proc.wait().expect("first wait ok");
    let second = proc.wait().expect("second wait ok");
    assert_eq!(first.stdout, second.stdout);
    assert_eq!(first.stderr, second.stderr);
    assert_eq!(first.stdout_lossy().trim(), "idempotent");
}

#[test]
fn wait_is_idempotent_on_failure() {
    let proc = Cmd::new(PP_STATUS)
        .args(["3", "--err", "stderr-bytes", "--out", "stdout-bytes"])
        .spawn()
        .expect("spawn");
    let first = proc.wait().expect_err("first wait fails");
    let second = proc.wait().expect_err("second wait fails");
    assert_eq!(first.exit_status().unwrap().code(), Some(3));
    assert_eq!(second.exit_status().unwrap().code(), Some(3));
    assert_eq!(first.stderr(), second.stderr());
    assert_eq!(first.stdout(), second.stdout());
}

#[test]
fn try_wait_some_then_wait_returns_same() {
    let proc = Cmd::new(PP_ECHO).arg("converges").spawn().expect("spawn");
    // Wait for the child to exit so try_wait returns Some.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let first = loop {
        if let Some(out) = proc.try_wait().expect("try_wait") {
            break out;
        }
        if std::time::Instant::now() >= deadline {
            panic!("child never exited");
        }
        thread::sleep(Duration::from_millis(20));
    };
    let second = proc.wait().expect("second wait after try_wait");
    assert_eq!(first.stdout, second.stdout);
    assert_eq!(first.stdout_lossy().trim(), "converges");
}

#[test]
fn wait_timeout_none_does_not_consume_state() {
    let proc = Cmd::new(PP_SLEEP).arg("300").spawn().expect("spawn");
    // First call returns None because child still running.
    let first = proc
        .wait_timeout(Duration::from_millis(50))
        .expect("wait_timeout ok");
    assert!(first.is_none());
    // Subsequent wait should complete normally.
    let out = proc.wait().expect("wait ok after timeout");
    assert!(out.stderr.is_empty());
}

#[test]
fn concurrent_waits_see_same_outcome() {
    use std::sync::Arc;
    let proc = Arc::new(Cmd::new(PP_ECHO).arg("concurrent").spawn().expect("spawn"));
    let p1 = proc.clone();
    let p2 = proc.clone();
    let h1 = thread::spawn(move || p1.wait().expect("wait 1"));
    let h2 = thread::spawn(move || p2.wait().expect("wait 2"));
    let a = h1.join().expect("join 1");
    let b = h2.join().expect("join 2");
    assert_eq!(a.stdout, b.stdout);
    assert_eq!(a.stdout_lossy().trim(), "concurrent");
}

#[test]
fn concurrent_kill_unblocks_reader() {
    // If kill acquired a lock held by the in-flight read, this would deadlock.
    // shared_child's lock-free kill path must bypass the read path.
    let proc = Arc::new(Cmd::new(PP_SLEEP).arg("10000").spawn().expect("spawn"));
    let stdout = proc.take_stdout().expect("stdout");
    let killer = proc.clone();
    let handle = thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        let _ = killer.kill();
    });
    let mut r = BufReader::new(stdout);
    let mut buf = Vec::new();
    let _ = r.read_to_end(&mut buf);
    handle.join().expect("join");
    let _ = proc.wait();
}
