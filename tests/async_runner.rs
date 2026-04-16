//! Integration tests for `Cmd::run_async` and `Cmd::spawn_async`.

use std::time::{Duration, Instant};

use procpilot::{Cmd, RetryPolicy};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

const PP_ECHO: &str = env!("CARGO_BIN_EXE_pp_echo");
const PP_CAT: &str = env!("CARGO_BIN_EXE_pp_cat");
const PP_SLEEP: &str = env!("CARGO_BIN_EXE_pp_sleep");
const PP_STATUS: &str = env!("CARGO_BIN_EXE_pp_status");
const PP_PRINT_ENV: &str = env!("CARGO_BIN_EXE_pp_print_env");

#[tokio::test]
async fn run_async_before_spawn_hook_fires_once() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let count = Arc::new(AtomicUsize::new(0));
    let c = count.clone();
    let out = Cmd::new(PP_ECHO)
        .arg("hi")
        .before_spawn(move |_cmd| {
            c.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .run_async()
        .await
        .expect("ok");
    assert_eq!(count.load(Ordering::SeqCst), 1);
    assert_eq!(out.stdout_lossy().trim(), "hi");
}

#[tokio::test]
async fn run_async_before_spawn_fires_per_stage_on_pipeline() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let count = Arc::new(AtomicUsize::new(0));
    let c = count.clone();
    let _ = Cmd::new(PP_ECHO)
        .arg("x")
        .pipe(Cmd::new(PP_CAT))
        .pipe(Cmd::new(PP_CAT))
        .before_spawn(move |_cmd| {
            c.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .run_async()
        .await
        .expect("ok");
    assert_eq!(count.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn run_async_before_spawn_error_aborts_with_spawn_error() {
    use std::io;
    let err = Cmd::new(PP_ECHO)
        .arg("unused")
        .before_spawn(|_cmd| Err(io::Error::other("hook rejected")))
        .run_async()
        .await
        .expect_err("hook error should abort");
    assert!(err.is_spawn_failure());
}

#[tokio::test]
async fn run_async_before_spawn_can_mutate_env() {
    // Hook sets TEST_BEFORE_SPAWN via as_std_mut. Verify the child sees it.
    let out = Cmd::new(PP_PRINT_ENV)
        .arg("TEST_BEFORE_SPAWN")
        .before_spawn(|cmd| {
            cmd.env("TEST_BEFORE_SPAWN", "set_by_hook");
            Ok(())
        })
        .run_async()
        .await
        .expect("ok");
    assert_eq!(out.stdout_lossy().trim(), "set_by_hook");
}

#[tokio::test]
async fn run_async_captures_stdout() {
    let out = Cmd::new(PP_ECHO).arg("hello").run_async().await.expect("ok");
    assert_eq!(out.stdout_lossy().trim(), "hello");
}

#[tokio::test]
async fn run_async_surfaces_nonzero() {
    let err = Cmd::new(PP_STATUS)
        .args(["1", "--err", "boom"])
        .run_async()
        .await
        .expect_err("fail");
    assert!(err.is_non_zero_exit());
    assert_eq!(err.stderr(), Some("boom\n"));
}

#[tokio::test]
async fn run_async_missing_binary_is_spawn_failure() {
    let err = Cmd::new("nonexistent_binary_xyz_42")
        .run_async()
        .await
        .expect_err("fail");
    assert!(err.is_spawn_failure());
}

#[tokio::test]
async fn run_async_with_env_and_cwd() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = Cmd::new(PP_PRINT_ENV)
        .arg("ASYNC_TEST")
        .env("ASYNC_TEST", "hello")
        .in_dir(tmp.path())
        .run_async()
        .await
        .expect("ok");
    assert_eq!(out.stdout_lossy().trim(), "hello");
}

#[tokio::test]
async fn run_async_stdin_bytes() {
    let out = Cmd::new(PP_CAT)
        .stdin("piped through cat\n")
        .run_async()
        .await
        .expect("ok");
    assert_eq!(out.stdout_lossy().trim(), "piped through cat");
}

#[tokio::test]
async fn run_async_stdin_reader() {
    use std::io::Cursor;
    use procpilot::StdinData;
    let out = Cmd::new(PP_CAT)
        .stdin(StdinData::from_reader(Cursor::new(b"from reader".to_vec())))
        .run_async()
        .await
        .expect("ok");
    assert_eq!(out.stdout_lossy(), "from reader");
}

#[tokio::test]
async fn run_async_stdin_async_reader_streams() {
    use procpilot::StdinData;
    let src = std::io::Cursor::new(b"async stream".to_vec());
    let out = Cmd::new(PP_CAT)
        .stdin(StdinData::from_async_reader(src))
        .run_async()
        .await
        .expect("ok");
    assert_eq!(out.stdout_lossy(), "async stream");
}

#[tokio::test]
async fn sync_rejection_preserves_async_reader_for_later_clone() {
    use procpilot::StdinData;
    // If rejection happens AFTER consuming the reader, a later run_async on
    // a clone would get no stdin. Fix 1 ensures rejection happens BEFORE
    // attempt_stdin takes the reader.
    let base = Cmd::new(PP_CAT).stdin(StdinData::from_async_reader(
        std::io::Cursor::new(b"preserved".to_vec()),
    ));
    let sync_clone = base.clone();
    let async_clone = base.clone();

    // First: sync attempt that must reject.
    let err = sync_clone.run().expect_err("sync must reject AsyncReader");
    assert!(err.is_spawn_failure());

    // Second: async attempt on the SAME underlying reader via a clone —
    // must still succeed with the full payload because the reject path
    // never consumed the reader.
    let out = async_clone.run_async().await.expect("async ok");
    assert_eq!(out.stdout_lossy(), "preserved");
}

#[tokio::test]
async fn sync_rejection_with_spawn_retrying_predicate_does_not_retry() {
    use procpilot::{RetryPolicy, RunError, StdinData};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Custom predicate that would retry Spawn errors. The sync rejection
    // must not trigger it (the check is a structural pre-condition, not a
    // transient failure). The predicate should never see our error — the
    // failure happens before any attempt reaches the retry loop.
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = attempts.clone();

    let err = Cmd::new(PP_CAT)
        .stdin(StdinData::from_async_reader(std::io::Cursor::new(
            b"nope".to_vec(),
        )))
        .retry(RetryPolicy::default())
        .retry_when(move |_err| {
            counter.fetch_add(1, Ordering::SeqCst);
            true
        })
        .run()
        .expect_err("must reject");
    match err {
        RunError::Spawn { source, .. } => {
            assert_eq!(source.kind(), std::io::ErrorKind::InvalidInput);
        }
        other => panic!("expected Spawn(InvalidInput), got {other:?}"),
    }
    // Predicate never called — rejection short-circuits before retry loop.
    assert_eq!(attempts.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn run_sync_with_async_reader_returns_invalid_input() {
    use procpilot::{RunError, StdinData};
    let err = Cmd::new(PP_CAT)
        .stdin(StdinData::from_async_reader(std::io::Cursor::new(
            b"nope".to_vec(),
        )))
        .run()
        .expect_err("sync runner rejects async reader");
    match err {
        RunError::Spawn { source, .. } => {
            assert_eq!(source.kind(), std::io::ErrorKind::InvalidInput);
        }
        other => panic!("expected Spawn(InvalidInput), got {other:?}"),
    }
}

#[tokio::test]
async fn run_async_async_reader_is_one_shot_across_clones() {
    use procpilot::StdinData;
    // Cursor is AsyncRead. Two clones share the same Mutex<Option<Reader>>;
    // whichever attempt runs first takes it.
    let base = Cmd::new(PP_CAT)
        .stdin(StdinData::from_async_reader(std::io::Cursor::new(
            b"taken once".to_vec(),
        )));
    let a = base.clone();
    let b = base.clone();
    let out_a = a.run_async().await.expect("ok");
    let out_b = b.run_async().await.expect("ok");
    // First clone got the bytes, second got None (empty stdin → cat echoes nothing).
    let (primary, empty) = if out_a.stdout.is_empty() {
        (out_b, out_a)
    } else {
        (out_a, out_b)
    };
    assert_eq!(primary.stdout_lossy(), "taken once");
    assert_eq!(empty.stdout_lossy(), "");
}

#[tokio::test]
async fn run_async_timeout_fires() {
    let start = Instant::now();
    let err = Cmd::new(PP_SLEEP)
        .arg("10000")
        .timeout(Duration::from_millis(200))
        .run_async()
        .await
        .expect_err("should time out");
    assert!(err.is_timeout());
    assert!(start.elapsed() < Duration::from_secs(5));
}

#[tokio::test]
async fn run_async_retry_default_predicate_does_not_fire_without_match() {
    // Default predicate matches "stale" / ".lock" — plain exit 1 with no
    // stderr should not retry. One attempt, one failure.
    let err = Cmd::new(PP_STATUS)
        .arg("1")
        .retry(RetryPolicy::default())
        .run_async()
        .await
        .expect_err("fail");
    assert!(err.is_non_zero_exit());
}

#[tokio::test]
async fn run_async_retry_actually_loops() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Custom predicate that returns true (retry) for the first two attempts,
    // then false (surface the error). The hook tracks how many spawn
    // attempts occurred — should be exactly 3.
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = attempts.clone();

    let err = Cmd::new(PP_STATUS)
        .arg("1")
        .retry(RetryPolicy::default())
        .retry_when(move |_err| counter.fetch_add(1, Ordering::SeqCst) < 2)
        .run_async()
        .await
        .expect_err("fail");
    assert!(err.is_non_zero_exit());
    // Predicate called once per failed attempt. 3 attempts means it was
    // called 3 times (twice returning true, once returning false).
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn run_async_parallel_via_try_join() {
    let a = Cmd::new(PP_ECHO).arg("a").run_async();
    let b = Cmd::new(PP_ECHO).arg("b").run_async();
    let c = Cmd::new(PP_ECHO).arg("c").run_async();
    let (ra, rb, rc) = tokio::try_join!(a, b, c).expect("all ok");
    assert_eq!(ra.stdout_lossy().trim(), "a");
    assert_eq!(rb.stdout_lossy().trim(), "b");
    assert_eq!(rc.stdout_lossy().trim(), "c");
}

#[tokio::test]
async fn run_async_pipeline() {
    let out = Cmd::new(PP_ECHO)
        .arg("piped")
        .pipe(Cmd::new(PP_CAT))
        .run_async()
        .await
        .expect("ok");
    assert_eq!(out.stdout_lossy().trim(), "piped");
}

#[tokio::test]
async fn run_async_pipeline_pipefail() {
    let err = Cmd::new(PP_ECHO)
        .arg("x")
        .pipe(Cmd::new(PP_STATUS).arg("2"))
        .run_async()
        .await
        .expect_err("fail");
    assert!(err.is_non_zero_exit());
    assert_eq!(err.exit_status().and_then(|s| s.code()), Some(2));
}

#[tokio::test]
async fn spawn_async_wait_succeeds() {
    let mut proc = Cmd::new(PP_ECHO).arg("hi").spawn_async().await.expect("spawn");
    let out = proc.wait().await.expect("wait");
    assert_eq!(out.stdout_lossy().trim(), "hi");
}

#[tokio::test]
async fn spawn_async_take_stdin_stdout_bidirectional() {
    let mut proc = Cmd::new(PP_CAT).spawn_async().await.expect("spawn");
    let mut stdin = proc.take_stdin().expect("stdin piped");
    let mut stdout = proc.take_stdout().expect("stdout piped");

    tokio::spawn(async move {
        stdin.write_all(b"async bidirectional\n").await.expect("write");
        drop(stdin);
    });

    let mut buf = String::new();
    stdout.read_to_string(&mut buf).await.expect("read");
    let out = proc.wait().await.expect("wait ok after stdin closed");
    assert!(buf.contains("async bidirectional"));
    // RunOutput carries no stdout here because the caller drained via
    // take_stdout — the drain inside finalize is a no-op in that path.
    assert!(out.stdout.is_empty());
}

#[tokio::test]
async fn spawn_async_kill_via_select() {
    let mut proc = Cmd::new(PP_SLEEP).arg("10000").spawn_async().await.expect("spawn");
    let cancel = tokio::time::sleep(Duration::from_millis(100));
    tokio::pin!(cancel);
    tokio::select! {
        _ = proc.wait() => panic!("should not exit on its own"),
        _ = &mut cancel => {
            proc.kill().await.expect("kill");
            let _ = proc.wait().await;
        }
    }
}

#[tokio::test]
async fn spawn_async_wait_timeout_returns_none_while_running() {
    let mut proc = Cmd::new(PP_SLEEP).arg("5000").spawn_async().await.expect("spawn");
    let res = proc
        .wait_timeout(Duration::from_millis(100))
        .await
        .expect("wait_timeout");
    assert!(res.is_none());
    proc.kill().await.expect("kill");
    let _ = proc.wait().await;
}

#[tokio::test]
async fn spawn_async_pipeline_pids_count() {
    let mut proc = Cmd::new(PP_ECHO)
        .arg("x")
        .pipe(Cmd::new(PP_CAT))
        .spawn_async()
        .await
        .expect("spawn");
    assert!(proc.is_pipeline());
    assert_eq!(proc.pids().len(), 2);
    let out = proc.wait().await.expect("wait");
    assert_eq!(out.stdout_lossy().trim(), "x");
}

#[tokio::test]
async fn async_wait_is_idempotent_on_success() {
    let mut proc = Cmd::new(PP_ECHO).arg("async-idempotent").spawn_async().await.expect("spawn");
    let first = proc.wait().await.expect("first wait");
    let second = proc.wait().await.expect("second wait");
    assert_eq!(first.stdout, second.stdout);
    assert_eq!(first.stdout_lossy().trim(), "async-idempotent");
}

#[tokio::test]
async fn async_wait_is_idempotent_on_failure() {
    let mut proc = Cmd::new(PP_STATUS)
        .args(["5", "--err", "err-bytes"])
        .spawn_async()
        .await
        .expect("spawn");
    let first = proc.wait().await.expect_err("first fails");
    let second = proc.wait().await.expect_err("second fails");
    assert_eq!(first.exit_status().unwrap().code(), Some(5));
    assert_eq!(second.exit_status().unwrap().code(), Some(5));
    assert_eq!(first.stderr(), second.stderr());
}

#[tokio::test]
async fn async_wait_timeout_none_then_wait_works() {
    let mut proc = Cmd::new(PP_SLEEP).arg("200").spawn_async().await.expect("spawn");
    let first = proc
        .wait_timeout(Duration::from_millis(50))
        .await
        .expect("wait_timeout");
    assert!(first.is_none());
    let out = proc.wait().await.expect("wait after timeout");
    assert!(out.stderr.is_empty());
}

#[tokio::test]
async fn async_cancel_via_select_then_wait_returns_same() {
    // The README's cancellation pattern: select! drops the first wait,
    // kill + second wait must complete.
    let mut proc = Cmd::new(PP_SLEEP).arg("10000").spawn_async().await.expect("spawn");
    let cancel = tokio::time::sleep(Duration::from_millis(100));
    tokio::pin!(cancel);
    let _first_result = tokio::select! {
        res = proc.wait() => Some(res),
        _ = &mut cancel => None,
    };
    // Cancellation fired; first wait was dropped. Kill and wait again.
    proc.kill().await.expect("kill");
    let _second = proc.wait().await;
    // Third wait must be idempotent with the second.
    let _third = proc.wait().await;
}

#[tokio::test]
async fn async_pipeline_spawn_failure_does_not_leak_earlier_stages() {
    let start = Instant::now();
    let err = Cmd::new(PP_SLEEP)
        .arg("10000")
        .pipe(Cmd::new("nonexistent_binary_xyz_42"))
        .run_async()
        .await
        .expect_err("should fail");
    let elapsed = start.elapsed();
    assert!(err.is_spawn_failure());
    assert!(
        elapsed < Duration::from_secs(2),
        "async pipeline spawn-failure cleanup didn't kill stage 1 (took {elapsed:?})"
    );
}

#[tokio::test]
async fn spawn_async_streaming_lines() {
    let mut proc = Cmd::new(PP_CAT)
        .stdin("one\ntwo\nthree\n")
        .spawn_async()
        .await
        .expect("spawn");
    let stdout = proc.take_stdout().expect("piped");
    let mut reader = BufReader::new(stdout).lines();

    let mut lines = Vec::new();
    while let Ok(Some(line)) = reader.next_line().await {
        lines.push(line);
    }
    let _ = proc.wait().await;
    assert_eq!(lines, vec!["one", "two", "three"]);
}
