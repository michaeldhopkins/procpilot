//! Integration tests for the `Cmd` builder. Uses mock binaries in
//! `src/bin/pp_*` (via `env!("CARGO_BIN_EXE_pp_*")`) to avoid platform
//! dependence on shell utilities.

use std::io::Cursor;
use std::time::{Duration, Instant};

use procpilot::{Cmd, Redirection, RetryPolicy, StdinData};

const PP_ECHO: &str = env!("CARGO_BIN_EXE_pp_echo");
const PP_CAT: &str = env!("CARGO_BIN_EXE_pp_cat");
const PP_SLEEP: &str = env!("CARGO_BIN_EXE_pp_sleep");
const PP_STATUS: &str = env!("CARGO_BIN_EXE_pp_status");
const PP_PRINT_ENV: &str = env!("CARGO_BIN_EXE_pp_print_env");
const PP_PRINT_ENV_MULTI: &str = env!("CARGO_BIN_EXE_pp_print_env_multi");
const PP_PWD: &str = env!("CARGO_BIN_EXE_pp_pwd");
const PP_SPAM: &str = env!("CARGO_BIN_EXE_pp_spam");

// --- basic capture ---

#[test]
fn captures_stdout() {
    let out = Cmd::new(PP_ECHO).arg("hello").run().expect("ok");
    assert_eq!(out.stdout_lossy().trim(), "hello");
}

#[test]
fn nonzero_exit_returns_typed_error() {
    let err = Cmd::new(PP_STATUS).arg("1").run().expect_err("fail");
    assert!(err.is_non_zero_exit());
    assert!(err.exit_status().is_some());
}

#[test]
fn captures_stderr_on_failure() {
    let err = Cmd::new(PP_STATUS)
        .args(["1", "--err", "err"])
        .run()
        .expect_err("fail");
    assert_eq!(err.stderr(), Some("err\n"));
}

#[test]
fn captures_stdout_bytes_on_failure() {
    let err = Cmd::new(PP_STATUS)
        .args(["1", "--out", "output"])
        .run()
        .expect_err("fail");
    let stdout = err.stdout().expect("stdout present");
    assert_eq!(String::from_utf8_lossy(stdout).trim(), "output");
}

#[test]
fn missing_binary_is_spawn_failure() {
    let err = Cmd::new("nonexistent_binary_xyz_42").run().expect_err("fail");
    assert!(err.is_spawn_failure());
}

// --- in_dir ---

#[test]
fn in_dir_sets_cwd() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = Cmd::new(PP_PWD).in_dir(tmp.path()).run().expect("ok");
    let got = std::path::Path::new(out.stdout_lossy().trim())
        .canonicalize()
        .expect("canon got");
    let want = tmp.path().canonicalize().expect("canon want");
    assert_eq!(got, want);
}

#[test]
fn in_dir_nonexistent_is_spawn_failure() {
    let err = Cmd::new(PP_ECHO)
        .arg("hi")
        .in_dir("/nonexistent_dir_xyz_42")
        .run()
        .expect_err("fail");
    assert!(err.is_spawn_failure());
}

// --- env ---

#[test]
fn env_sets_single() {
    let out = Cmd::new(PP_PRINT_ENV)
        .arg("TEST_VAR")
        .env("TEST_VAR", "hello")
        .run()
        .expect("ok");
    assert_eq!(out.stdout_lossy().trim(), "hello");
}

#[test]
fn env_sets_multiple_same_spawn() {
    let out = Cmd::new(PP_PRINT_ENV_MULTI)
        .args(["A", "B"])
        .envs([("A", "foo"), ("B", "bar")])
        .run()
        .expect("ok");
    assert_eq!(out.stdout_lossy().trim(), "foo bar");
}

#[test]
fn env_overrides_existing_var() {
    let out = Cmd::new(PP_PRINT_ENV)
        .arg("HOME")
        .env("HOME", "/fake/home")
        .run()
        .expect("ok");
    assert_eq!(out.stdout_lossy().trim(), "/fake/home");
}

// --- stdin ---

#[test]
fn stdin_bytes_fed_to_child() {
    let out = Cmd::new(PP_CAT).stdin("piped input").run().expect("ok");
    assert_eq!(out.stdout_lossy(), "piped input");
}

#[test]
fn stdin_reader_one_shot() {
    let cursor = Cursor::new(b"from reader".to_vec());
    let out = Cmd::new(PP_CAT)
        .stdin(StdinData::from_reader(cursor))
        .run()
        .expect("ok");
    assert_eq!(out.stdout_lossy(), "from reader");
}

#[test]
fn stdin_vec_bytes() {
    let out = Cmd::new(PP_CAT)
        .stdin(vec![b'h', b'i'])
        .run()
        .expect("ok");
    assert_eq!(out.stdout_lossy(), "hi");
}

// --- timeout ---

#[test]
fn timeout_fast_command_succeeds() {
    let out = Cmd::new(PP_ECHO)
        .arg("hi")
        .timeout(Duration::from_secs(5))
        .run()
        .expect("ok");
    assert_eq!(out.stdout_lossy().trim(), "hi");
}

#[test]
fn timeout_fires_for_slow_command() {
    let start = Instant::now();
    let err = Cmd::new(PP_SLEEP)
        .arg("10000")
        .timeout(Duration::from_millis(200))
        .run()
        .expect_err("fail");
    assert!(err.is_timeout());
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[test]
fn timeout_captures_partial_stderr() {
    let err = Cmd::new(PP_STATUS)
        .args(["1", "--err", "partial", "--sleep-ms", "10000"])
        .timeout(Duration::from_millis(1500))
        .run()
        .expect_err("fail");
    assert!(err.is_timeout());
    let stderr = err.stderr().unwrap_or("");
    assert!(stderr.contains("partial"), "got: {stderr:?}");
}

#[test]
fn timeout_does_not_block_on_large_output() {
    let out = Cmd::new(PP_SPAM)
        .arg("200000")
        .timeout(Duration::from_secs(5))
        .run()
        .expect("ok");
    assert!(out.stdout.len() >= 200_000);
}

// --- deadline ---

#[test]
fn deadline_overrides_timeout_when_tighter() {
    let start = Instant::now();
    let err = Cmd::new(PP_SLEEP)
        .arg("10000")
        .timeout(Duration::from_secs(60))
        .deadline(Instant::now() + Duration::from_millis(200))
        .run()
        .expect_err("fail");
    assert!(err.is_timeout());
    assert!(start.elapsed() < Duration::from_secs(2));
}

// --- retry ---

#[test]
fn retry_does_not_fire_on_non_transient_error() {
    // Plain non-zero exit with no matching stderr shouldn't trigger the default predicate.
    let err = Cmd::new(PP_STATUS)
        .arg("1")
        .retry(RetryPolicy::default())
        .run()
        .expect_err("fail");
    assert!(err.is_non_zero_exit());
}

#[test]
fn retry_when_custom_predicate_can_stop_retrying() {
    let err = Cmd::new(PP_STATUS)
        .arg("1")
        .retry_when(|_| false)
        .run()
        .expect_err("fail");
    assert!(err.is_non_zero_exit());
}

// --- stderr redirection ---

#[test]
fn stderr_null_discards() {
    let err = Cmd::new(PP_STATUS)
        .args(["1", "--err", "dropped"])
        .stderr(Redirection::Null)
        .run()
        .expect_err("fail");
    // stderr was discarded, so the captured field is empty.
    assert_eq!(err.stderr(), Some(""));
}

// --- secret ---

#[test]
fn secret_redacts_in_error_display() {
    let err = Cmd::new("nonexistent_binary_xyz_42")
        .arg("sensitive")
        .secret()
        .run()
        .expect_err("fail");
    let msg = format!("{err}");
    assert!(!msg.contains("sensitive"), "secret leaked: {msg}");
    assert!(msg.contains("<secret>"));
}

// --- binary helpers ---

#[test]
fn binary_available_true_for_exit_zero() {
    assert!(procpilot::binary_available(PP_ECHO));
}

#[test]
fn binary_available_false_for_missing() {
    assert!(!procpilot::binary_available("nonexistent_binary_xyz_42"));
}

#[test]
fn binary_version_none_for_missing() {
    assert!(procpilot::binary_version("nonexistent_binary_xyz_42").is_none());
}

// --- before_spawn ---

#[test]
fn before_spawn_hook_invoked() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    let count = Arc::new(AtomicUsize::new(0));
    let c = count.clone();
    let out = Cmd::new(PP_ECHO)
        .arg("hi")
        .before_spawn(move |_cmd| {
            c.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .run()
        .expect("ok");
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(out.stdout_lossy().trim(), "hi");
}
