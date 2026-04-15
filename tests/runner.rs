//! Integration tests for procpilot's runner. Uses mock binaries in `src/bin/pp_*`
//! (referenced via `env!("CARGO_BIN_EXE_pp_*")`) to avoid platform dependence on
//! shell utilities.

use std::time::{Duration, Instant};

use procpilot::{
    RunError, run_cmd, run_cmd_in, run_cmd_in_with_env, run_cmd_in_with_timeout,
    run_cmd_inherited,
};

// Mock binary paths, resolved at test compile time by cargo.
const PP_ECHO: &str = env!("CARGO_BIN_EXE_pp_echo");
const PP_SLEEP: &str = env!("CARGO_BIN_EXE_pp_sleep");
const PP_STATUS: &str = env!("CARGO_BIN_EXE_pp_status");
const PP_PRINT_ENV: &str = env!("CARGO_BIN_EXE_pp_print_env");
const PP_PRINT_ENV_MULTI: &str = env!("CARGO_BIN_EXE_pp_print_env_multi");
const PP_PWD: &str = env!("CARGO_BIN_EXE_pp_pwd");
const PP_SPAM: &str = env!("CARGO_BIN_EXE_pp_spam");

// --- run_cmd_inherited ---

#[test]
fn cmd_inherited_succeeds() {
    run_cmd_inherited(PP_STATUS, &["0"]).expect("pp_status 0 should succeed");
}

#[test]
fn cmd_inherited_fails_on_nonzero() {
    let err = run_cmd_inherited(PP_STATUS, &["1"]).expect_err("should fail");
    assert!(err.is_non_zero_exit());
    assert_eq!(err.program(), PP_STATUS);
}

#[test]
fn cmd_inherited_fails_on_missing_binary() {
    let err = run_cmd_inherited("nonexistent_binary_xyz_42", &[]).expect_err("should fail");
    assert!(err.is_spawn_failure());
}

// --- run_cmd ---

#[test]
fn cmd_captured_succeeds() {
    let output = run_cmd(PP_ECHO, &["hello"]).expect("pp_echo should succeed");
    assert_eq!(output.stdout_lossy().trim(), "hello");
}

#[test]
fn cmd_captured_fails_on_nonzero() {
    let err = run_cmd(PP_STATUS, &["1"]).expect_err("should fail");
    assert!(err.is_non_zero_exit());
    assert!(err.exit_status().is_some());
}

#[test]
fn cmd_captured_captures_stderr_on_failure() {
    let err = run_cmd(PP_STATUS, &["1", "--err", "err"]).expect_err("should fail");
    assert_eq!(err.stderr(), Some("err\n"));
}

#[test]
fn cmd_captured_captures_stdout_on_failure() {
    let err = run_cmd(PP_STATUS, &["1", "--out", "output"]).expect_err("should fail");
    match &err {
        RunError::NonZeroExit { stdout, .. } => {
            assert_eq!(String::from_utf8_lossy(stdout).trim(), "output");
        }
        _ => panic!("expected NonZeroExit"),
    }
}

#[test]
fn cmd_fails_on_missing_binary() {
    let err = run_cmd("nonexistent_binary_xyz_42", &[]).expect_err("should fail");
    assert!(err.is_spawn_failure());
}

// --- run_cmd_in ---

#[test]
fn cmd_in_runs_in_directory() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let output = run_cmd_in(tmp.path(), PP_PWD, &[]).expect("pp_pwd should work");
    let pwd = output.stdout_lossy().trim().to_string();
    let expected = tmp.path().canonicalize().expect("canonicalize");
    let actual = std::path::Path::new(&pwd)
        .canonicalize()
        .expect("canonicalize pwd");
    assert_eq!(actual, expected);
}

#[test]
fn cmd_in_fails_on_nonzero() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let err = run_cmd_in(tmp.path(), PP_STATUS, &["1"]).expect_err("should fail");
    assert!(err.is_non_zero_exit());
}

#[test]
fn cmd_in_fails_on_nonexistent_dir() {
    let err = run_cmd_in(
        std::path::Path::new("/nonexistent_dir_xyz_42"),
        PP_ECHO,
        &["hi"],
    )
    .expect_err("should fail");
    assert!(err.is_spawn_failure());
}

// --- run_cmd_in_with_env ---

#[test]
fn cmd_in_with_env_sets_variable() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let output = run_cmd_in_with_env(
        tmp.path(),
        PP_PRINT_ENV,
        &["TEST_VAR_XYZ"],
        &[("TEST_VAR_XYZ", "hello_from_env")],
    )
    .expect("should succeed");
    assert_eq!(output.stdout_lossy().trim(), "hello_from_env");
}

#[test]
fn cmd_in_with_env_multiple_vars_same_invocation() {
    // Proves BOTH vars reach the child in the same spawn (not just one at a
    // time across two spawns).
    let tmp = tempfile::tempdir().expect("tempdir");
    let output = run_cmd_in_with_env(
        tmp.path(),
        PP_PRINT_ENV_MULTI,
        &["A", "B"],
        &[("A", "foo"), ("B", "bar")],
    )
    .expect("should succeed");
    assert_eq!(output.stdout_lossy().trim(), "foo bar");
}

#[test]
fn cmd_in_with_env_overrides_existing_var() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let output = run_cmd_in_with_env(
        tmp.path(),
        PP_PRINT_ENV,
        &["HOME"],
        &[("HOME", "/fake/home")],
    )
    .expect("should succeed");
    assert_eq!(output.stdout_lossy().trim(), "/fake/home");
}

#[test]
fn cmd_in_with_env_fails_on_nonzero() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let err = run_cmd_in_with_env(
        tmp.path(),
        PP_STATUS,
        &["1"],
        &[("IRRELEVANT", "value")],
    )
    .expect_err("should fail");
    assert!(err.is_non_zero_exit());
}

// --- run_cmd_in_with_timeout ---

#[test]
fn timeout_succeeds_for_fast_command() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let output = run_cmd_in_with_timeout(tmp.path(), PP_ECHO, &["hello"], Duration::from_secs(5))
        .expect("should succeed");
    assert_eq!(output.stdout_lossy().trim(), "hello");
}

#[test]
fn timeout_fires_for_slow_command() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let wall_start = Instant::now();
    let err = run_cmd_in_with_timeout(
        tmp.path(),
        PP_SLEEP,
        &["10000"],
        Duration::from_millis(200),
    )
    .expect_err("should time out");
    let wall_elapsed = wall_start.elapsed();

    assert!(err.is_timeout());
    assert!(
        wall_elapsed < Duration::from_secs(5),
        "expected quick kill, took {wall_elapsed:?}"
    );
}

#[test]
fn timeout_captures_partial_stderr_before_kill() {
    let tmp = tempfile::tempdir().expect("tempdir");
    // pp_status 1 --err partial --sleep-ms 10000 writes "partial" to stderr
    // then sleeps 10s. The 500ms timeout fires first; stderr was already
    // flushed so it's captured.
    let err = run_cmd_in_with_timeout(
        tmp.path(),
        PP_STATUS,
        &["1", "--err", "partial", "--sleep-ms", "10000"],
        Duration::from_millis(500),
    )
    .expect_err("should time out");
    assert!(err.is_timeout());
    let stderr = err.stderr().unwrap_or("");
    assert!(
        stderr.contains("partial"),
        "expected partial stderr, got: {stderr:?}"
    );
}

#[test]
fn timeout_reports_non_zero_exit_when_process_completes() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let err = run_cmd_in_with_timeout(tmp.path(), PP_STATUS, &["1"], Duration::from_secs(5))
        .expect_err("should fail");
    assert!(err.is_non_zero_exit());
}

#[test]
fn timeout_fails_on_missing_binary() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let err = run_cmd_in_with_timeout(
        tmp.path(),
        "nonexistent_binary_xyz_42",
        &[],
        Duration::from_secs(5),
    )
    .expect_err("should fail");
    assert!(err.is_spawn_failure());
}

#[test]
fn timeout_does_not_block_on_large_output() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let output =
        run_cmd_in_with_timeout(tmp.path(), PP_SPAM, &["200000"], Duration::from_secs(5))
            .expect("should succeed");
    assert!(output.stdout.len() >= 200_000);
}

// --- check_output ---

#[test]
fn check_output_preserves_stderr_on_success() {
    let output =
        run_cmd(PP_STATUS, &["0", "--out", "ok", "--err", "warn"]).expect("should succeed");
    assert_eq!(output.stdout_lossy().trim(), "ok");
    assert_eq!(output.stderr.trim(), "warn");
}

// --- binary_available / binary_version ---
//
// `binary_available(name)` currently returns true iff spawning `name --version`
// succeeds with exit 0. It's a weak heuristic — any binary that exits 0 on
// arbitrary args will pass. We test the positive path with a binary whose
// exit-0-on-any-args behavior matches what the heuristic actually detects.

#[test]
fn binary_available_returns_true_when_binary_exits_zero() {
    // pp_echo always exits 0. This exercises the spawn-and-check-status path.
    assert!(procpilot::binary_available(PP_ECHO));
}

#[test]
fn binary_available_missing_returns_false() {
    assert!(!procpilot::binary_available("nonexistent_binary_xyz_42"));
}

#[test]
fn binary_version_missing_returns_none() {
    assert!(procpilot::binary_version("nonexistent_binary_xyz_42").is_none());
}
