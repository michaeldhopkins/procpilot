//! Async variants of [`Cmd::run`] and [`Cmd::spawn`], gated on the `tokio`
//! feature.
//!
//! Mirrors the sync execution path using `tokio::process::Command` and
//! `tokio::time::timeout`. Pipelines are supported via the same
//! [`os_pipe`]-based fd plumbing; intermediate pipe halves are attached to
//! `tokio::process::Command` through `Stdio::from`, which works because
//! tokio reuses `std::process::Stdio`.

use std::process::Stdio;
use std::time::{Duration, Instant};

use backon::Retryable;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, Command};

use super::{
    BeforeSpawnHook, Cmd, CmdTree, Outcome, RunOutput, SingleCmd, AsyncStdinForAttempt,
    attempt_stdin_async, combine_outcomes, finalize_outcome,
    reject_non_capture_stdout_on_spawn,
};
use crate::async_spawned::AsyncSpawnedProcess;
use crate::cmd_display::CmdDisplay;
use crate::error::RunError;
use crate::redirection::Redirection;

impl Cmd {
    /// Run the command (or pipeline) asynchronously, awaiting completion.
    ///
    /// Mirrors [`Cmd::run`]: all builder knobs apply (args, env, cwd,
    /// stdin, stderr redirection, timeout, deadline, retry, secret).
    /// `before_spawn` is currently ignored on the async path — see the
    /// top-level README for the full async parity list.
    #[cfg_attr(docsrs, doc(cfg(feature = "tokio")))]
    pub async fn run_async(mut self) -> Result<RunOutput, RunError> {
        let display = self.display();
        let stdin = self.stdin.take();
        let retry = self.retry.take();
        let timeout = self.timeout;
        let deadline = self.deadline;
        let stdout_mode = self.stdout_mode.clone();
        let stderr_mode = self.stderr_mode.clone();
        let before_spawn = self.before_spawn.clone();
        let tree = self.tree;

        let op = || {
            let display = &display;
            let stdin = &stdin;
            let stdout_mode = &stdout_mode;
            let stderr_mode = &stderr_mode;
            let before_spawn = before_spawn.as_ref();
            let tree = &tree;
            async move {
                let now = Instant::now();
                if let Some(d) = deadline
                    && now >= d
                {
                    return Err(RunError::Timeout {
                        command: display.clone(),
                        elapsed: Duration::ZERO,
                        stdout: Vec::new(),
                        stderr: String::new(),
                    });
                }
                let per_attempt = match (timeout, deadline) {
                    (None, None) => None,
                    (Some(t), None) => Some(t),
                    (None, Some(d)) => Some(d.saturating_duration_since(now)),
                    (Some(t), Some(d)) => Some(t.min(d.saturating_duration_since(now))),
                };
                let stdin_attempt = attempt_stdin_async(stdin);
                match tree {
                    CmdTree::Single(s) => {
                        execute_single_async(
                            s,
                            stdout_mode,
                            stderr_mode,
                            before_spawn,
                            display,
                            stdin_attempt,
                            per_attempt,
                        )
                        .await
                    }
                    CmdTree::Pipe(_, _) => {
                        let mut stages = Vec::new();
                        tree.flatten(&mut stages);
                        execute_pipeline_async(
                            &stages,
                            stdout_mode,
                            stderr_mode,
                            before_spawn,
                            display,
                            stdin_attempt,
                            per_attempt,
                        )
                        .await
                    }
                }
            }
        };

        match retry {
            None => op().await,
            Some(policy) => {
                let predicate = policy.predicate.clone();
                op.retry(policy.backoff)
                    .when(move |e: &RunError| predicate(e))
                    .await
            }
        }
    }

    /// Spawn asynchronously, returning an [`AsyncSpawnedProcess`] handle.
    ///
    /// Stdin and stdout are always piped. Stderr routing follows
    /// [`Redirection`]. `timeout`, `deadline`, and `retry` do not apply
    /// on the spawn path — use [`AsyncSpawnedProcess::wait_timeout`] or
    /// [`AsyncSpawnedProcess::kill`] for per-call bounds.
    #[cfg_attr(docsrs, doc(cfg(feature = "tokio")))]
    pub async fn spawn_async(mut self) -> Result<AsyncSpawnedProcess, RunError> {
        let display = self.display();
        let stdin_shared = self.stdin.take();
        reject_non_capture_stdout_on_spawn(&self.stdout_mode, &display)?;
        let stdin_attempt = attempt_stdin_async(&stdin_shared);
        let before_spawn = self.before_spawn.as_ref();
        let mut stages = Vec::new();
        flatten_tree_owned(self.tree, &mut stages);
        match stages.len() {
            1 => {
                spawn_single_async(
                    stages.into_iter().next().expect("len == 1"),
                    &self.stderr_mode,
                    before_spawn,
                    stdin_attempt,
                    display,
                )
                .await
            }
            _ => {
                spawn_pipeline_async(
                    stages,
                    &self.stderr_mode,
                    before_spawn,
                    stdin_attempt,
                    display,
                )
                .await
            }
        }
    }
}

fn flatten_tree_owned(tree: CmdTree, out: &mut Vec<SingleCmd>) {
    let mut stack: Vec<CmdTree> = vec![tree];
    while let Some(node) = stack.pop() {
        match node {
            CmdTree::Single(s) => out.push(s),
            CmdTree::Pipe(l, r) => {
                stack.push(*r);
                stack.push(*l);
            }
        }
    }
}

fn apply_single_to_tokio_command(single: &SingleCmd, cmd: &mut Command) {
    cmd.args(&single.args);
    if let Some(d) = &single.cwd {
        cmd.current_dir(d);
    }
    if single.env_clear {
        cmd.env_clear();
    }
    for k in &single.env_remove {
        cmd.env_remove(k);
    }
    for (k, v) in &single.envs {
        cmd.env(k, v);
    }
    // Every async Child we spawn kills its OS process on drop. Normal
    // success paths explicitly wait the child (so the kill is a no-op);
    // mid-pipeline spawn failures drop already-spawned children on the
    // error-return path, and kill_on_drop ensures those processes don't
    // leak as orphans.
    cmd.kill_on_drop(true);
}

fn run_before_spawn_tokio(
    cmd: &mut Command,
    hook: Option<&BeforeSpawnHook>,
    display: &CmdDisplay,
) -> Result<(), RunError> {
    if let Some(h) = hook {
        // tokio::process::Command wraps std::process::Command; as_std_mut
        // exposes the inner handle so the existing sync-shaped hook works
        // unchanged. Tokio-specific options (e.g., kill_on_drop) are not
        // part of the std handle — procpilot manages those separately.
        h(cmd.as_std_mut()).map_err(|source| RunError::Spawn {
            command: display.clone(),
            source,
        })?;
    }
    Ok(())
}

fn apply_stdout_tokio(
    cmd: &mut Command,
    mode: &Redirection,
    display: &CmdDisplay,
) -> Result<(), RunError> {
    match mode {
        Redirection::Capture => {
            cmd.stdout(Stdio::piped());
        }
        Redirection::Inherit => {
            cmd.stdout(Stdio::inherit());
        }
        Redirection::Null => {
            cmd.stdout(Stdio::null());
        }
        Redirection::File(f) => {
            let cloned = f.as_ref().try_clone().map_err(|source| RunError::Spawn {
                command: display.clone(),
                source,
            })?;
            cmd.stdout(Stdio::from(cloned));
        }
    }
    Ok(())
}

fn apply_stderr_tokio(
    cmd: &mut Command,
    mode: &Redirection,
    display: &CmdDisplay,
) -> Result<(), RunError> {
    match mode {
        Redirection::Capture => {
            cmd.stderr(Stdio::piped());
        }
        Redirection::Inherit => {
            cmd.stderr(Stdio::inherit());
        }
        Redirection::Null => {
            cmd.stderr(Stdio::null());
        }
        Redirection::File(f) => {
            let cloned = f.as_ref().try_clone().map_err(|source| RunError::Spawn {
                command: display.clone(),
                source,
            })?;
            cmd.stderr(Stdio::from(cloned));
        }
    }
    Ok(())
}

async fn drain_async<R: tokio::io::AsyncRead + Unpin>(mut r: R) -> Vec<u8> {
    let mut buf = Vec::new();
    let _ = r.read_to_end(&mut buf).await;
    buf
}

fn spawn_stdin_feeder_tokio(pipe: ChildStdin, stdin: AsyncStdinForAttempt) {
    let mut pipe = pipe;
    tokio::spawn(async move {
        match stdin {
            AsyncStdinForAttempt::None => {}
            AsyncStdinForAttempt::Bytes(bytes) => {
                let _ = pipe.write_all(&bytes).await;
            }
            AsyncStdinForAttempt::Reader(reader) => {
                // Sync Read source on the async runner: the only correct
                // way to drive it is from a blocking thread. For large
                // inputs this buffers the full contents in memory; prefer
                // `StdinData::from_async_reader` for streaming.
                let bytes = tokio::task::spawn_blocking(move || {
                    use std::io::Read;
                    let mut reader = reader;
                    let mut buf = Vec::new();
                    let _ = reader.read_to_end(&mut buf);
                    buf
                })
                .await
                .unwrap_or_default();
                let _ = pipe.write_all(&bytes).await;
            }
            AsyncStdinForAttempt::AsyncReader(mut reader) => {
                // True async streaming: tokio::io::copy pumps chunks from
                // the reader to the child's pipe without buffering.
                let _ = tokio::io::copy(&mut reader, &mut pipe).await;
            }
        }
    });
}

async fn execute_single_async(
    single: &SingleCmd,
    stdout_mode: &Redirection,
    stderr_mode: &Redirection,
    before_spawn: Option<&BeforeSpawnHook>,
    display: &CmdDisplay,
    stdin: AsyncStdinForAttempt,
    timeout: Option<Duration>,
) -> Result<RunOutput, RunError> {
    let mut cmd = Command::new(&single.program);
    apply_single_to_tokio_command(single, &mut cmd);
    let piped_stdin = !matches!(stdin, AsyncStdinForAttempt::None);
    if piped_stdin {
        cmd.stdin(Stdio::piped());
    }
    apply_stdout_tokio(&mut cmd, stdout_mode, display)?;
    apply_stderr_tokio(&mut cmd, stderr_mode, display)?;
    run_before_spawn_tokio(&mut cmd, before_spawn, display)?;

    let mut child = cmd.spawn().map_err(|source| RunError::Spawn {
        command: display.clone(),
        source,
    })?;

    if piped_stdin && let Some(pipe) = child.stdin.take() {
        spawn_stdin_feeder_tokio(pipe, stdin);
    }

    let stdout_task = if matches!(stdout_mode, Redirection::Capture) {
        let stdout = child.stdout.take().expect("stdout piped");
        Some(tokio::spawn(async move { drain_async(stdout).await }))
    } else {
        None
    };
    let stderr_task = if matches!(stderr_mode, Redirection::Capture) {
        let stderr = child.stderr.take().expect("stderr piped");
        Some(tokio::spawn(async move { drain_async(stderr).await }))
    } else {
        None
    };

    let start = Instant::now();
    let outcome = match timeout {
        Some(t) => match tokio::time::timeout(t, child.wait()).await {
            Ok(Ok(status)) => Outcome::Exited(status),
            Ok(Err(source)) => Outcome::WaitFailed(source),
            Err(_) => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                Outcome::TimedOut(start.elapsed())
            }
        },
        None => match child.wait().await {
            Ok(status) => Outcome::Exited(status),
            Err(source) => Outcome::WaitFailed(source),
        },
    };

    let stdout_bytes = match stdout_task {
        Some(t) => t.await.unwrap_or_default(),
        None => Vec::new(),
    };
    let stderr_bytes = match stderr_task {
        Some(t) => t.await.unwrap_or_default(),
        None => Vec::new(),
    };
    let stderr_str = String::from_utf8_lossy(&stderr_bytes).into_owned();

    finalize_outcome(display, outcome, stdout_bytes, stderr_str)
}

async fn execute_pipeline_async(
    stages: &[&SingleCmd],
    stdout_mode: &Redirection,
    stderr_mode: &Redirection,
    before_spawn: Option<&BeforeSpawnHook>,
    display: &CmdDisplay,
    stdin: AsyncStdinForAttempt,
    timeout: Option<Duration>,
) -> Result<RunOutput, RunError> {
    debug_assert!(stages.len() >= 2);

    let mut pipes: Vec<(Option<os_pipe::PipeReader>, Option<os_pipe::PipeWriter>)> =
        Vec::new();
    for _ in 0..stages.len() - 1 {
        let (r, w) = os_pipe::pipe().map_err(|source| RunError::Spawn {
            command: display.clone(),
            source,
        })?;
        pipes.push((Some(r), Some(w)));
    }

    let mut children: Vec<Child> = Vec::with_capacity(stages.len());
    let mut stderr_tasks: Vec<tokio::task::JoinHandle<Vec<u8>>> = Vec::new();
    let mut last_stdout = None;
    let mut stdin_for_feed = Some(stdin);

    for (i, stage) in stages.iter().enumerate() {
        let mut cmd = Command::new(&stage.program);
        apply_single_to_tokio_command(stage, &mut cmd);

        if i == 0 {
            match stdin_for_feed.as_ref() {
                Some(AsyncStdinForAttempt::None) | None => {}
                Some(_) => {
                    cmd.stdin(Stdio::piped());
                }
            }
        } else {
            let reader = pipes[i - 1].0.take().expect("pipe reader");
            cmd.stdin(Stdio::from(reader));
        }

        if i == stages.len() - 1 {
            apply_stdout_tokio(&mut cmd, stdout_mode, display)?;
        } else {
            let writer = pipes[i].1.take().expect("pipe writer");
            cmd.stdout(Stdio::from(writer));
        }

        apply_stderr_tokio(&mut cmd, stderr_mode, display)?;
        run_before_spawn_tokio(&mut cmd, before_spawn, display)?;

        let mut child = cmd.spawn().map_err(|source| RunError::Spawn {
            command: display.clone(),
            source,
        })?;

        if i == 0
            && let Some(data) = stdin_for_feed.take()
            && !matches!(data, AsyncStdinForAttempt::None)
            && let Some(pipe) = child.stdin.take()
        {
            spawn_stdin_feeder_tokio(pipe, data);
        }

        if matches!(stderr_mode, Redirection::Capture)
            && let Some(stderr) = child.stderr.take()
        {
            stderr_tasks.push(tokio::spawn(async move { drain_async(stderr).await }));
        }

        if i == stages.len() - 1 && matches!(stdout_mode, Redirection::Capture) {
            last_stdout = child.stdout.take();
        }

        children.push(child);
    }

    let stdout_task = last_stdout.map(|s| tokio::spawn(async move { drain_async(s).await }));

    let start = Instant::now();
    let mut per_stage_status: Vec<Outcome> = Vec::with_capacity(children.len());

    if let Some(budget) = timeout {
        for child in children.iter_mut() {
            let remaining = budget.saturating_sub(start.elapsed());
            if remaining.is_zero() {
                let _ = child.kill().await;
                let _ = child.wait().await;
                per_stage_status.push(Outcome::TimedOut(start.elapsed()));
                continue;
            }
            match tokio::time::timeout(remaining, child.wait()).await {
                Ok(Ok(status)) => per_stage_status.push(Outcome::Exited(status)),
                Ok(Err(source)) => per_stage_status.push(Outcome::WaitFailed(source)),
                Err(_) => {
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    per_stage_status.push(Outcome::TimedOut(start.elapsed()));
                }
            }
        }
    } else {
        for child in children.iter_mut() {
            match child.wait().await {
                Ok(status) => per_stage_status.push(Outcome::Exited(status)),
                Err(source) => per_stage_status.push(Outcome::WaitFailed(source)),
            }
        }
    }

    let stdout_bytes = match stdout_task {
        Some(t) => t.await.unwrap_or_default(),
        None => Vec::new(),
    };
    let mut stderr_all = String::new();
    for t in stderr_tasks {
        let bytes = t.await.unwrap_or_default();
        stderr_all.push_str(&String::from_utf8_lossy(&bytes));
    }

    let final_outcome = combine_outcomes(per_stage_status);
    finalize_outcome(display, final_outcome, stdout_bytes, stderr_all)
}

async fn spawn_single_async(
    single: SingleCmd,
    stderr_mode: &Redirection,
    before_spawn: Option<&BeforeSpawnHook>,
    stdin: AsyncStdinForAttempt,
    display: CmdDisplay,
) -> Result<AsyncSpawnedProcess, RunError> {
    let mut cmd = Command::new(&single.program);
    apply_single_to_tokio_command(&single, &mut cmd);
    cmd.stdin(Stdio::piped());
    cmd.stdout(Stdio::piped());
    apply_stderr_tokio(&mut cmd, stderr_mode, &display)?;
    run_before_spawn_tokio(&mut cmd, before_spawn, &display)?;

    let mut child = cmd.spawn().map_err(|source| RunError::Spawn {
        command: display.clone(),
        source,
    })?;

    if !matches!(stdin, AsyncStdinForAttempt::None)
        && let Some(pipe) = child.stdin.take()
    {
        spawn_stdin_feeder_tokio(pipe, stdin);
    }

    let stderr_task = if matches!(stderr_mode, Redirection::Capture) {
        child
            .stderr
            .take()
            .map(|s| tokio::spawn(async move { drain_async(s).await }))
    } else {
        None
    };

    Ok(AsyncSpawnedProcess::new_single(
        child,
        stderr_task,
        display,
    ))
}

async fn spawn_pipeline_async(
    stages: Vec<SingleCmd>,
    stderr_mode: &Redirection,
    before_spawn: Option<&BeforeSpawnHook>,
    mut stdin: AsyncStdinForAttempt,
    display: CmdDisplay,
) -> Result<AsyncSpawnedProcess, RunError> {
    let mut pipes: Vec<(Option<os_pipe::PipeReader>, Option<os_pipe::PipeWriter>)> =
        Vec::new();
    for _ in 0..stages.len() - 1 {
        let (r, w) = os_pipe::pipe().map_err(|source| RunError::Spawn {
            command: display.clone(),
            source,
        })?;
        pipes.push((Some(r), Some(w)));
    }

    let mut children: Vec<Child> = Vec::with_capacity(stages.len());
    let mut stderr_tasks: Vec<tokio::task::JoinHandle<Vec<u8>>> = Vec::new();

    for (i, stage) in stages.iter().enumerate() {
        let mut cmd = Command::new(&stage.program);
        apply_single_to_tokio_command(stage, &mut cmd);

        if i == 0 {
            cmd.stdin(Stdio::piped());
        } else {
            let reader = pipes[i - 1].0.take().expect("pipe reader");
            cmd.stdin(Stdio::from(reader));
        }

        if i == stages.len() - 1 {
            cmd.stdout(Stdio::piped());
        } else {
            let writer = pipes[i].1.take().expect("pipe writer");
            cmd.stdout(Stdio::from(writer));
        }

        apply_stderr_tokio(&mut cmd, stderr_mode, &display)?;
        run_before_spawn_tokio(&mut cmd, before_spawn, &display)?;

        let mut child = cmd.spawn().map_err(|source| RunError::Spawn {
            command: display.clone(),
            source,
        })?;

        if i == 0 {
            let attempt = std::mem::replace(&mut stdin, AsyncStdinForAttempt::None);
            if !matches!(attempt, AsyncStdinForAttempt::None)
                && let Some(pipe) = child.stdin.take()
            {
                spawn_stdin_feeder_tokio(pipe, attempt);
            }
        }

        if matches!(stderr_mode, Redirection::Capture)
            && let Some(stderr) = child.stderr.take()
        {
            stderr_tasks.push(tokio::spawn(async move { drain_async(stderr).await }));
        }

        children.push(child);
    }

    Ok(AsyncSpawnedProcess::new_pipeline(
        children,
        stderr_tasks,
        display,
    ))
}
