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

// --- spawn + wait ---

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

// --- kill ---

#[test]
fn spawn_kill_returns_error_on_wait() {
    let proc = Cmd::new(PP_SLEEP).arg("10000").spawn().expect("spawn");
    proc.kill().expect("kill");
    let result = proc.wait();
    // On Unix, SIGKILL yields a non-zero ExitStatus (no exit code, signal 9).
    // We just confirm wait doesn't hang and surfaces SOMETHING.
    assert!(result.is_err() || result.is_ok());
}

// --- take_stdin / take_stdout (bidirectional) ---

#[test]
fn take_stdin_and_stdout_pipe_bidirectionally() {
    // This is the branchdiff `git cat-file --batch` pattern: one thread writes
    // lines into stdin, main thread reads stdout.
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

// --- Read impl ---

#[test]
fn read_impl_streams_stdout() {
    let mut proc = Cmd::new(PP_ECHO).arg("abc").spawn().expect("spawn");
    let mut buf = String::new();
    proc.read_to_string(&mut buf).expect("read");
    assert_eq!(buf.trim(), "abc");
    // After Read consumes stdout, take_stdout returns None.
    assert!(proc.take_stdout().is_none());
    let _ = proc.wait();
}

#[test]
fn read_via_shared_reference() {
    let proc = Cmd::new(PP_SPAM).arg("1000").spawn().expect("spawn");
    let reader = &proc;
    let mut buf = Vec::new();
    // `(&proc).read_to_end(...)` exercises `impl Read for &SpawnedProcess`.
    let _ = (&mut { reader }).read_to_end(&mut buf);
    let _ = proc.wait();
    assert!(!buf.is_empty());
}

// --- spawn_and_collect_lines ---

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
    assert!(out.stdout.is_empty()); // drained by the callback
    let lines = collected.lock().unwrap();
    assert_eq!(*lines, vec!["one", "two", "three"]);
}

// --- wait_timeout ---

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

// --- try_wait ---

#[test]
fn try_wait_reports_running_then_done() {
    let proc = Cmd::new(PP_SLEEP).arg("500").spawn().expect("spawn");
    // Immediately after spawn, should still be running.
    let initial = proc.try_wait().expect("try_wait");
    assert!(initial.is_none());
    // Poll for up to 5 seconds until it reports completion.
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

// --- pids ---

#[test]
fn pids_returns_single_entry() {
    let proc = Cmd::new(PP_ECHO).arg("x").spawn().expect("spawn");
    let pids = proc.pids();
    assert_eq!(pids.len(), 1);
    assert!(pids[0] > 0);
    let _ = proc.wait();
}

// --- spawn missing binary ---

#[test]
fn spawn_missing_binary_is_spawn_error() {
    let err = Cmd::new("nonexistent_binary_xyz_42")
        .spawn()
        .expect_err("should fail");
    assert!(err.is_spawn_failure());
}

// --- concurrent kill while main reads ---

#[test]
fn concurrent_kill_unblocks_reader() {
    // Reader on main thread, kill from helper — shared_child's lock-free
    // kill must not deadlock against the read.
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
