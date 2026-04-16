//! Integration tests for pipelines (`Cmd::pipe` / `BitOr`).

use std::io::{Read, Write};
use std::time::Duration;

use procpilot::Cmd;

const PP_ECHO: &str = env!("CARGO_BIN_EXE_pp_echo");
const PP_CAT: &str = env!("CARGO_BIN_EXE_pp_cat");
const PP_STATUS: &str = env!("CARGO_BIN_EXE_pp_status");
const PP_SLEEP: &str = env!("CARGO_BIN_EXE_pp_sleep");
const PP_SPAM: &str = env!("CARGO_BIN_EXE_pp_spam");

#[test]
fn two_stage_pipe_passes_stdout_to_stdin() {
    let out = Cmd::new(PP_ECHO)
        .arg("hello")
        .pipe(Cmd::new(PP_CAT))
        .run()
        .expect("ok");
    assert_eq!(out.stdout_lossy().trim(), "hello");
}

#[test]
fn bitor_operator_produces_same_result() {
    let out = (Cmd::new(PP_ECHO).arg("via-bitor") | Cmd::new(PP_CAT))
        .run()
        .expect("ok");
    assert_eq!(out.stdout_lossy().trim(), "via-bitor");
}

#[test]
fn three_stage_pipeline_runs_in_order() {
    let out = Cmd::new(PP_ECHO)
        .arg("staged")
        .pipe(Cmd::new(PP_CAT))
        .pipe(Cmd::new(PP_CAT))
        .run()
        .expect("ok");
    assert_eq!(out.stdout_lossy().trim(), "staged");
}

#[test]
fn pipefail_rightmost_failure_wins() {
    let err = Cmd::new(PP_ECHO)
        .arg("x")
        .pipe(Cmd::new(PP_STATUS).arg("2"))
        .run()
        .expect_err("should fail");
    assert!(err.is_non_zero_exit());
    assert_eq!(err.exit_status().and_then(|s| s.code()), Some(2));
}

#[test]
fn pipefail_middle_failure_surfaces_when_later_stages_succeed() {
    // Pipefail picks the rightmost non-success; when only the middle stage
    // fails, that failure must still surface — not silently hidden by the
    // later stage's success.
    let err = Cmd::new(PP_STATUS)
        .args(["7", "--out", "ignored"])
        .pipe(Cmd::new(PP_CAT))
        .run()
        .expect_err("should fail");
    assert!(err.is_non_zero_exit());
    assert_eq!(err.exit_status().and_then(|s| s.code()), Some(7));
}

#[test]
fn stdin_feeds_leftmost_stage() {
    let out = Cmd::new(PP_CAT)
        .pipe(Cmd::new(PP_CAT))
        .stdin("piped through two cats\n")
        .run()
        .expect("ok");
    assert_eq!(out.stdout_lossy().trim(), "piped through two cats");
}

#[test]
fn stderr_captures_from_all_stages_concatenated() {
    let err = Cmd::new(PP_STATUS)
        .args(["0", "--err", "first-err"])
        .pipe(Cmd::new(PP_STATUS).args(["1", "--err", "second-err"]))
        .run()
        .expect_err("rightmost non-zero should fail");
    let stderr = err.stderr().unwrap_or("");
    assert!(stderr.contains("first-err"), "got: {stderr:?}");
    assert!(stderr.contains("second-err"), "got: {stderr:?}");
}

#[test]
fn pipeline_timeout_kills_hung_stage() {
    let err = Cmd::new(PP_SLEEP)
        .arg("10000")
        .pipe(Cmd::new(PP_CAT))
        .timeout(Duration::from_millis(200))
        .run()
        .expect_err("should time out");
    assert!(err.is_timeout());
}

#[test]
fn pipeline_does_not_deadlock_on_large_output() {
    let out = Cmd::new(PP_SPAM)
        .arg("100000")
        .pipe(Cmd::new(PP_CAT))
        .run()
        .expect("ok");
    assert!(out.stdout.len() >= 100_000);
}

#[test]
fn args_after_pipe_target_rightmost() {
    // pp_echo ignores stdin, so the final output comes from the rightmost
    // stage's args — proving that .arg("overrides") attached to the correct
    // (rightmost) stage rather than to the first.
    let out = Cmd::new(PP_ECHO)
        .arg("first")
        .pipe(Cmd::new(PP_CAT))
        .pipe(Cmd::new(PP_ECHO))
        .arg("overrides")
        .run()
        .expect("ok");
    assert_eq!(out.stdout_lossy().trim(), "overrides");
}

#[test]
fn spawn_on_pipeline_returns_all_pids() {
    let proc = Cmd::new(PP_ECHO)
        .arg("hi")
        .pipe(Cmd::new(PP_CAT))
        .spawn()
        .expect("spawn");
    assert!(proc.is_pipeline());
    assert_eq!(proc.pids().len(), 2);
    let out = proc.wait().expect("wait");
    assert_eq!(out.stdout_lossy().trim(), "hi");
}

#[test]
fn spawn_pipeline_bidirectional_take_stdin_and_stdout() {
    let proc = Cmd::new(PP_CAT)
        .pipe(Cmd::new(PP_CAT))
        .spawn()
        .expect("spawn");
    let mut stdin = proc.take_stdin().expect("stdin");
    let mut stdout = proc.take_stdout().expect("stdout");

    let writer = std::thread::spawn(move || {
        stdin.write_all(b"piped via two cats").expect("write");
        drop(stdin);
    });

    let mut buf = String::new();
    stdout.read_to_string(&mut buf).expect("read");
    writer.join().expect("join");
    let _ = proc.wait();
    assert_eq!(buf, "piped via two cats");
}

#[test]
fn spawn_pipeline_kill_sends_to_all_stages() {
    let proc = Cmd::new(PP_SLEEP)
        .arg("10000")
        .pipe(Cmd::new(PP_CAT))
        .spawn()
        .expect("spawn");
    proc.kill().expect("kill");
    // If kill only reached the first stage, wait would hang on pp_cat waiting
    // for its stdin to close — so this line is the real assertion.
    let _ = proc.wait();
}

#[test]
fn spawn_pipeline_pipefail_on_wait() {
    let proc = Cmd::new(PP_ECHO)
        .arg("x")
        .pipe(Cmd::new(PP_STATUS).arg("3"))
        .spawn()
        .expect("spawn");
    let err = proc.wait().expect_err("should fail");
    assert!(err.is_non_zero_exit());
    assert_eq!(err.exit_status().and_then(|s| s.code()), Some(3));
}

#[test]
fn pipeline_spawn_failure_does_not_leak_earlier_stages() {
    use std::time::Instant;
    // First stage is a long-sleep; second stage is a missing binary.
    // Without cleanup, the sleep would linger for 10 seconds while this
    // test completes. With cleanup, the killed child lets .run() return
    // quickly.
    let start = Instant::now();
    let err = Cmd::new(PP_SLEEP)
        .arg("10000")
        .pipe(Cmd::new("nonexistent_binary_xyz_42"))
        .run()
        .expect_err("should fail");
    let elapsed = start.elapsed();
    assert!(err.is_spawn_failure());
    // If cleanup works, we return as soon as the second spawn fails.
    // Permit 2s for slow CI.
    assert!(
        elapsed < Duration::from_secs(2),
        "pipeline spawn-failure cleanup didn't kill stage 1 (took {elapsed:?})"
    );
}

#[test]
fn spawn_pipeline_spawn_failure_kills_earlier_stages() {
    use std::time::Instant;
    let start = Instant::now();
    let err = Cmd::new(PP_SLEEP)
        .arg("10000")
        .pipe(Cmd::new("nonexistent_binary_xyz_42"))
        .spawn()
        .expect_err("should fail");
    let elapsed = start.elapsed();
    assert!(err.is_spawn_failure());
    assert!(
        elapsed < Duration::from_secs(2),
        "spawn pipeline cleanup didn't kill stage 1 (took {elapsed:?})"
    );
}

#[test]
fn display_renders_shell_style_pipeline() {
    let cmd = Cmd::new("git").arg("log").pipe(Cmd::new("grep").arg("feat"));
    let d = cmd.display();
    assert_eq!(d.to_string(), "git log | grep feat");
}
