//! Integration tests for pipelines (`Cmd::pipe` / `BitOr`).

use std::time::Duration;

use procpilot::{Cmd, RunError};

const PP_ECHO: &str = env!("CARGO_BIN_EXE_pp_echo");
const PP_CAT: &str = env!("CARGO_BIN_EXE_pp_cat");
const PP_STATUS: &str = env!("CARGO_BIN_EXE_pp_status");
const PP_SLEEP: &str = env!("CARGO_BIN_EXE_pp_sleep");
const PP_SPAM: &str = env!("CARGO_BIN_EXE_pp_spam");

// --- basic pipe: a | b ---

#[test]
fn two_stage_pipe_passes_stdout_to_stdin() {
    // pp_echo prints "hello"; pp_cat echoes stdin to stdout.
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

// --- three-stage pipeline ---

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

// --- pipefail: rightmost failure wins ---

#[test]
fn pipefail_rightmost_failure_wins() {
    // echo (ok) | status 2 (fail)  →  error with exit status 2
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
    // status 7 (fail) | cat (ok)  →  failure with exit status 7 wins
    // (rightmost NON-SUCCESS wins; cat succeeds, status 7 is the only failure)
    let err = Cmd::new(PP_STATUS)
        .args(["7", "--out", "ignored"])
        .pipe(Cmd::new(PP_CAT))
        .run()
        .expect_err("should fail");
    assert!(err.is_non_zero_exit());
    assert_eq!(err.exit_status().and_then(|s| s.code()), Some(7));
}

// --- stdin feeds first stage ---

#[test]
fn stdin_feeds_leftmost_stage() {
    let out = Cmd::new(PP_CAT)
        .pipe(Cmd::new(PP_CAT))
        .stdin("piped through two cats\n")
        .run()
        .expect("ok");
    assert_eq!(out.stdout_lossy().trim(), "piped through two cats");
}

// --- stderr capture concatenates from all stages ---

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

// --- timeout on pipeline ---

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

// --- large output doesn't deadlock ---

#[test]
fn pipeline_does_not_deadlock_on_large_output() {
    let out = Cmd::new(PP_SPAM)
        .arg("100000")
        .pipe(Cmd::new(PP_CAT))
        .run()
        .expect("ok");
    assert!(out.stdout.len() >= 100_000);
}

// --- env/args target rightmost after pipe ---

#[test]
fn args_after_pipe_target_rightmost() {
    // Cmd::new(PP_ECHO) pipes to PP_CAT with no args; but if we do
    // .pipe(PP_ECHO).arg("X"), then rightmost PP_ECHO gets "X".
    let out = Cmd::new(PP_ECHO)
        .arg("first")
        .pipe(Cmd::new(PP_CAT))
        .pipe(Cmd::new(PP_ECHO))
        .arg("overrides")
        .run()
        .expect("ok");
    // PP_ECHO at the end ignores stdin and prints "overrides".
    assert_eq!(out.stdout_lossy().trim(), "overrides");
}

// --- spawn on pipeline returns Unsupported ---

#[test]
fn spawn_on_pipeline_is_unsupported() {
    let err = Cmd::new(PP_ECHO)
        .pipe(Cmd::new(PP_CAT))
        .spawn()
        .expect_err("pipelines can't spawn yet");
    match err {
        RunError::Spawn { source, .. } => {
            assert_eq!(source.kind(), std::io::ErrorKind::Unsupported);
        }
        _ => panic!("expected Spawn unsupported, got {err:?}"),
    }
}

// --- display renders pipeline ---

#[test]
fn display_renders_shell_style_pipeline() {
    let cmd = Cmd::new("git").arg("log").pipe(Cmd::new("grep").arg("feat"));
    let d = cmd.display();
    assert_eq!(d.to_string(), "git log | grep feat");
}
