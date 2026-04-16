//! End-to-end test of the `Runner` / `MockRunner` pattern from a
//! downstream-consumer perspective.

use std::path::Path;

use procpilot::testing::{MockRunner, nonzero, ok_str, spawn_error};
use procpilot::{Cmd, RunError, Runner};

/// Production-style helper that takes `&dyn Runner` so it's testable
/// without spawning a real `git`.
fn current_branch(runner: &dyn Runner, repo: &Path) -> Result<String, RunError> {
    let cmd = Cmd::new("git")
        .args(["branch", "--show-current"])
        .in_dir(repo);
    let out = runner.run(cmd)?;
    Ok(out.stdout_lossy().trim().to_string())
}

#[test]
fn mock_returns_canned_stdout() {
    let mock = MockRunner::new().expect("git branch --show-current", ok_str("main\n"));
    let branch = current_branch(&mock, Path::new("/tmp")).expect("ok");
    assert_eq!(branch, "main");
    mock.verify().expect("all expectations met");
}

#[test]
fn mock_returns_canned_error() {
    let mock = MockRunner::new()
        .expect("git fetch", nonzero(128, "fatal: unable to access remote"));
    let err = (&mock as &dyn Runner)
        .run(Cmd::new("git").arg("fetch"))
        .expect_err("mock returned Err");
    assert!(err.is_non_zero_exit());
    assert_eq!(err.exit_status().and_then(|s| s.code()), Some(128));
    assert_eq!(err.stderr(), Some("fatal: unable to access remote"));
    // The mock substitutes the actual command's display into the error.
    assert_eq!(err.command().to_string(), "git fetch");
}

#[test]
fn predicate_matcher_inspects_cwd_via_to_rightmost_command() {
    let mock = MockRunner::new().expect_when(
        |cmd| {
            let std_cmd = cmd.to_rightmost_command();
            std_cmd.get_program() == "git"
                && std_cmd.get_current_dir() == Some(Path::new("/special-repo"))
        },
        ok_str("on the special repo"),
    );
    let out = mock
        .run(Cmd::new("git").arg("status").in_dir("/special-repo"))
        .expect("matched by cwd");
    assert_eq!(out.stdout_lossy(), "on the special repo");
}

#[test]
fn verify_reports_unmet_expectations() {
    let mock = MockRunner::new()
        .expect("git status", ok_str(""))
        .expect("git log", ok_str(""));
    let _ = mock.run(Cmd::new("git").arg("status"));
    let report = mock.verify().expect_err("one expectation never matched");
    assert!(report.contains("git log"), "got: {report}");
}

#[test]
fn first_match_wins_among_overlapping_expectations() {
    let mock = MockRunner::new()
        .expect_when(|_| true, ok_str("first"))
        .expect_when(|_| true, ok_str("second"));
    let a = mock.run(Cmd::new("anything")).unwrap();
    let b = mock.run(Cmd::new("anything")).unwrap();
    assert_eq!(a.stdout_lossy(), "first");
    assert_eq!(b.stdout_lossy(), "second");
    mock.verify().unwrap();
}

#[test]
#[should_panic(expected = "no matching expectation")]
fn no_match_panics_by_default() {
    let mock = MockRunner::new().expect("git status", ok_str(""));
    let _ = mock.run(Cmd::new("git").arg("log"));
}

#[test]
fn no_match_returns_spawn_error_when_configured() {
    let mock = MockRunner::new()
        .error_on_no_match()
        .expect("git status", ok_str(""));
    let err = mock
        .run(Cmd::new("git").arg("log"))
        .expect_err("no match returns Err");
    assert!(err.is_spawn_failure());
    assert!(err.command().to_string().contains("git log"));
}

#[test]
fn spawn_error_helper_constructs_typed_error() {
    let mock = MockRunner::new().expect("missing-binary", spawn_error("not on PATH"));
    let err = mock
        .run(Cmd::new("missing-binary"))
        .expect_err("err");
    assert!(err.is_spawn_failure());
}

#[test]
fn panic_on_no_match_does_not_poison_mutex() {
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::Arc;

    let mock = Arc::new(MockRunner::new().expect("matches", ok_str("ok")));

    // First call: no match → panics. The MutexGuard is alive at the
    // panic site; without the fix, unwinding poisons the mutex.
    let m = Arc::clone(&mock);
    let _ = catch_unwind(AssertUnwindSafe(|| {
        let _ = m.run(Cmd::new("does-not-match"));
    }));

    // Second call: should find the "matches" expectation and return ok.
    // If the mutex is poisoned, `.lock().expect(...)` inside run() panics
    // with "MockRunner mutex poisoned", which the catch_unwind below
    // captures as Err — failing the assertion.
    let result = catch_unwind(AssertUnwindSafe(|| mock.run(Cmd::new("matches"))));
    assert!(
        result.is_ok(),
        "run() after a caught no-match panic should not re-panic due to a poisoned mutex"
    );
    let out = result.expect("no re-panic").expect("canned ok result");
    assert_eq!(out.stdout_lossy(), "ok");
}

#[test]
fn expect_repeated_matches_up_to_n_times() {
    use procpilot::testing::ok_str;
    let mock = MockRunner::new().expect_repeated("git pull", 3, || ok_str("Already up to date."));
    for _ in 0..3 {
        let out = mock.run(Cmd::new("git").arg("pull")).expect("ok");
        assert_eq!(out.stdout_lossy(), "Already up to date.");
    }
    // 4th call should not match the expectation (exhausted).
    let err = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        mock.run(Cmd::new("git").arg("pull"))
    }));
    assert!(err.is_err(), "4th call should panic on no-match");
}

#[test]
fn expect_always_matches_unlimited_times() {
    use procpilot::testing::ok_str;
    let mock = MockRunner::new().expect_always("git config core.hooksPath", || {
        ok_str(".husky\n")
    });
    for _ in 0..10 {
        let out = mock
            .run(Cmd::new("git").args(["config", "core.hooksPath"]))
            .expect("ok");
        assert_eq!(out.stdout_lossy(), ".husky\n");
    }
    mock.verify().expect("always-matching expectation counts as met after first call");
}

#[test]
fn expect_repeated_with_varying_factory_output() {
    use procpilot::testing::ok_str;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    let counter = Arc::new(AtomicUsize::new(0));
    let c = counter.clone();
    let mock = MockRunner::new().expect_repeated("git log", 3, move || {
        let n = c.fetch_add(1, Ordering::SeqCst);
        ok_str(format!("commit {n}"))
    });
    let a = mock.run(Cmd::new("git").arg("log")).unwrap();
    let b = mock.run(Cmd::new("git").arg("log")).unwrap();
    let c2 = mock.run(Cmd::new("git").arg("log")).unwrap();
    assert_eq!(a.stdout_lossy(), "commit 0");
    assert_eq!(b.stdout_lossy(), "commit 1");
    assert_eq!(c2.stdout_lossy(), "commit 2");
}

#[test]
fn mock_result_resolve_attaches_given_command() {
    // Custom Runner impl uses MockResult helpers correctly: `resolve`
    // inserts a command display of our choosing. The placeholder from
    // the old design is gone.
    use procpilot::CmdDisplay;
    use procpilot::testing::{MockResult, nonzero};

    // Reach CmdDisplay via Cmd::display since we don't construct it
    // directly from test code.
    let cmd = Cmd::new("pretend").arg("arg");
    let display: CmdDisplay = cmd.display();
    let result: MockResult = nonzero(2, "something failed");
    let err = result
        .resolve(&display)
        .expect_err("nonzero resolves to Err");
    assert!(err.is_non_zero_exit());
    assert_eq!(err.command().to_string(), "pretend arg");
}

#[test]
fn mock_result_resolve_covers_every_variant() {
    use procpilot::testing::{MockResult, nonzero, ok, ok_str, spawn_error, timeout};
    use std::time::Duration;
    let cmd = Cmd::new("x").display();

    // Ok / ok_str
    let out = ok(b"bytes".to_vec()).resolve(&cmd).expect("Ok");
    assert_eq!(out.stdout, b"bytes");
    let out = ok_str("hello").resolve(&cmd).expect("Ok");
    assert_eq!(out.stdout_lossy(), "hello");

    // NonZeroExit
    let err = nonzero(42, "nope").resolve(&cmd).expect_err("Err");
    assert!(err.is_non_zero_exit());
    assert_eq!(err.exit_status().and_then(|s| s.code()), Some(42));

    // Spawn
    let err = spawn_error("boom").resolve(&cmd).expect_err("Err");
    assert!(err.is_spawn_failure());

    // Timeout
    let err = timeout(Duration::from_secs(1), "hung").resolve(&cmd).expect_err("Err");
    assert!(err.is_timeout());

    // If this test fails to compile (not: fails at runtime) because of a
    // missing match arm, a new MockResult variant was added; update
    // resolve() to handle it.
    let _compile_check: fn(MockResult, &procpilot::CmdDisplay) = |r, d| {
        let _ = r.resolve(d);
    };
}

#[test]
fn pipeline_display_matches_for_mock() {
    let mock = MockRunner::new().expect("git log | head -5", ok_str("commit ..."));
    let out = mock
        .run(Cmd::new("git").arg("log").pipe(Cmd::new("head").arg("-5")))
        .expect("ok");
    assert_eq!(out.stdout_lossy(), "commit ...");
}
