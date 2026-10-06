//! No child outlives the handle, the run, or the test that started it.
//!
//! - A dropped [`SpawnedProcess`] kills and reaps every stage still running.
//! - A one-shot reader taken by an earlier run leaves later runs an empty stdin, not the
//!   parent's: an inherited stdin that never closes left `pp_cat` children waiting forever.
//! - A mock whose test process dies exits on its own (`src/bin/support/orphan.rs`).
//!
//! Unix-only: liveness is read from `ps`.

#![cfg(unix)]

#[path = "support/liveness.rs"]
mod liveness;

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use liveness::{alive, gone_within_deadline};
use procpilot::Cmd;
use wait_timeout::ChildExt;

const PP_SLEEP: &str = env!("CARGO_BIN_EXE_pp_sleep");
const PP_CAT: &str = env!("CARGO_BIN_EXE_pp_cat");
const PP_RERUN_READER: &str = env!("CARGO_BIN_EXE_pp_rerun_reader");
const PP_CHILD_GRANDCHILD: &str = env!("CARGO_BIN_EXE_pp_child_grandchild");

/// A plain `std` child killed and reaped when the test unwinds.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn dropping_a_spawned_process_kills_and_reaps_it() {
    let proc = Cmd::new(PP_SLEEP).arg("30000").spawn().expect("spawn");
    let pid = proc.pids()[0];
    assert!(alive(pid), "the child should be running before the drop");
    drop(proc);
    assert!(gone_within_deadline(pid), "the dropped handle left pid {pid} running");
}

#[test]
fn dropping_a_spawned_pipeline_kills_every_stage() {
    let proc = Cmd::new(PP_SLEEP)
        .arg("30000")
        .pipe(Cmd::new(PP_SLEEP).arg("30000"))
        .spawn()
        .expect("spawn");
    let pids = proc.pids();
    drop(proc);
    for pid in pids {
        assert!(gone_within_deadline(pid), "the dropped pipeline left stage {pid} running");
    }
}

#[test]
fn dropping_a_finished_spawned_process_is_harmless() {
    let proc = Cmd::new(PP_SLEEP).arg("0").spawn().expect("spawn");
    let pid = proc.pids()[0];
    proc.wait_timeout(Duration::from_secs(10))
        .expect("wait")
        .expect("pp_sleep 0 exits at once");
    drop(proc);
    assert!(!alive(pid));
}

#[cfg(feature = "tokio")]
#[tokio::test]
async fn dropping_an_async_spawned_process_kills_it() {
    let proc = Cmd::new(PP_SLEEP).arg("30000").spawn_async().await.expect("spawn");
    let pid = proc.pids()[0];
    drop(proc);
    let gone = tokio::task::spawn_blocking(move || gone_within_deadline(pid))
        .await
        .expect("join");
    assert!(gone, "the dropped async handle left pid {pid} running");
}

/// Runs `pp_rerun_reader` with a stdin pipe that stays open and silent, and returns what its
/// second `pp_cat` read. A second run that inherited that stdin would never see EOF.
fn second_run_stdin(mode: &str) -> String {
    let (reader, writer) = os_pipe::pipe().expect("pipe");
    let mut probe = KillOnDrop(
        Command::new(PP_RERUN_READER)
            .args([PP_CAT, mode])
            .stdin(reader)
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn pp_rerun_reader"),
    );
    let status = probe.0.wait_timeout(Duration::from_secs(10)).expect("wait");
    // Closing our end gives any pp_cat still reading it EOF, so a failure leaves nothing behind.
    drop(writer);
    let status = status.unwrap_or_else(|| {
        panic!("{mode}: the second run read the parent's stdin and never finished")
    });
    assert!(status.success(), "{mode}: pp_rerun_reader failed: {status}");
    let mut out = String::new();
    probe.0.stdout.take().expect("stdout").read_to_string(&mut out).expect("read");
    out
}

#[test]
fn a_taken_reader_leaves_a_later_run_an_empty_stdin() {
    assert_eq!(second_run_stdin("run"), "");
}

#[test]
fn a_taken_reader_leaves_a_later_pipeline_an_empty_stdin() {
    assert_eq!(second_run_stdin("pipeline"), "");
}

#[cfg(feature = "tokio")]
#[test]
fn a_taken_async_reader_leaves_a_later_async_run_an_empty_stdin() {
    assert_eq!(second_run_stdin("async"), "");
}

#[test]
fn a_taken_reader_leaves_a_later_spawn_an_empty_stdin() {
    let cmd = Cmd::new(PP_CAT).stdin(procpilot::StdinData::from_reader(std::io::Cursor::new(
        b"taken once".to_vec(),
    )));
    let first = cmd.clone().spawn().expect("first spawn");
    let second = cmd.spawn().expect("second spawn");
    let first = first.wait_timeout(Duration::from_secs(10)).expect("wait").expect("first exits");
    let second = second
        .wait_timeout(Duration::from_secs(10))
        .expect("wait")
        .expect("the second pp_cat never saw EOF");
    assert_eq!(first.stdout_lossy(), "taken once");
    assert_eq!(second.stdout_lossy(), "");
}

#[test]
fn a_mock_whose_parent_dies_exits_on_its_own() {
    let dir = tempfile::tempdir().expect("tempdir");
    let sentinel = dir.path().join("grandchild.pid");
    let mut outer = KillOnDrop(
        Command::new(PP_CHILD_GRANDCHILD)
            .arg("30000")
            .arg(format!("--sentinel={}", sentinel.display()))
            .stdin(Stdio::null())
            .spawn()
            .expect("spawn"),
    );
    let grandchild = read_pid_within_deadline(&sentinel);
    outer.0.kill().expect("kill the parent");
    outer.0.wait().expect("reap the parent");
    assert!(
        gone_within_deadline(grandchild),
        "orphaned mock {grandchild} kept running"
    );
}

fn read_pid_within_deadline(path: &std::path::Path) -> u32 {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(pid) = std::fs::read_to_string(path)
            .ok()
            .and_then(|s| s.trim().parse().ok())
        {
            return pid;
        }
        assert!(std::time::Instant::now() < deadline, "no pid in {}", path.display());
        std::thread::sleep(Duration::from_millis(25));
    }
}
