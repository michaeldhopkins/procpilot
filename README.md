# procpilot

Subprocess runner for Rust. Typed errors, retry, timeout, stdin piping, pipelines, secret redaction, optional async.

## What it does

- `RunError` with `Spawn`, `NonZeroExit`, and `Timeout` variants (captured stdout/stderr on the latter two).
- Retry via `backon` (exponential backoff + jitter) with a configurable predicate.
- `.timeout()` per attempt, `.deadline()` across all attempts.
- Stdin from owned bytes (reusable across retries) or a boxed `Read` (one-shot).
- Stdout/stderr routing: capture, inherit, null, redirect to file. `Cmd::run` honors both; `Cmd::spawn` always pipes stdout so the handle can expose it.
- `.secret()` replaces args with `<secret>` in error output and logs.
- `.spawn()` returns a `SpawnedProcess` with `take_stdin` / `take_stdout`, `Read` impls, `kill`, `wait`, `wait_timeout`, and `spawn_and_collect_lines`.
- Pipelines via `.pipe()` or `|`, executed with pipefail status precedence.
- `Cmd: Clone` for base-plus-variants usage; `impl Display for Cmd`.
- Async (`.run_async()`, `.spawn_async()`) behind the `tokio` feature.

## Usage

```toml
[dependencies]
procpilot = "0.6"
```

For async (tokio) users:

```toml
[dependencies]
procpilot = { version = "0.6", features = ["tokio"] }
```

```rust
use std::time::Duration;
use procpilot::{Cmd, RunError};

let output = Cmd::new("git")
    .args(["fetch", "origin"])
    .in_dir("/repo")
    .env("GIT_TERMINAL_PROMPT", "0")
    .timeout(Duration::from_secs(30))
    .run()?;
# Ok::<(), procpilot::RunError>(())
```

For codebases that reach for procpilot's types frequently, a `prelude` is available:

```rust
use procpilot::prelude::*;

let _: Cmd = Cmd::new("git").stderr(Redirection::Inherit);
```

### Reusing a base `Cmd`

`Cmd` is `Clone`, so you can build a base configuration once and branch off variants:

```rust
use procpilot::Cmd;

let base = Cmd::new("git").in_dir("/repo").env("GIT_TERMINAL_PROMPT", "0");
let status = base.clone().args(["status", "--short"]).run()?;
let log    = base.clone().args(["log", "-1", "--oneline"]).run()?;
# Ok::<(), procpilot::RunError>(())
```

### Error handling

```rust
use procpilot::{Cmd, RunError};

match Cmd::new("git").args(["show", "maybe-missing-ref"]).run() {
    Ok(output) => Some(output.stdout),
    Err(RunError::NonZeroExit { .. }) => None,   // legitimate in-band signal
    Err(e) => return Err(e.into()),
}
# ; Ok::<(), anyhow::Error>(())
```

`RunError` is `#[non_exhaustive]`; include a wildcard arm. All variants carry a [`CmdDisplay`] that renders the command shell-style (with secret redaction if `.secret()` was set). `NonZeroExit` and `Timeout` include up to the last 128 KiB of stdout/stderr.

### Retry

```rust
use procpilot::{Cmd, RunError, RetryPolicy};

Cmd::new("git")
    .args(["pull"])
    .in_dir("/repo")
    .retry(RetryPolicy::default())
    .retry_when(|err| matches!(err, RunError::NonZeroExit { stderr, .. } if stderr.contains(".lock")))
    .run()?;
# Ok::<(), RunError>(())
```

### Deadline across retries

`.timeout()` bounds a single attempt; `.deadline()` bounds the whole operation (including retry backoff sleeps).

```rust
use std::time::{Duration, Instant};
use procpilot::{Cmd, RetryPolicy};

Cmd::new("git")
    .args(["fetch", "origin"])
    .in_dir("/repo")
    .timeout(Duration::from_secs(3))
    .deadline(Instant::now() + Duration::from_secs(10))
    .retry(RetryPolicy::default())
    .run()?;
# Ok::<(), procpilot::RunError>(())
```

### Inheriting stderr

Route the child's stderr to the parent's stderr (instead of capturing) with `Redirection::Inherit`. Useful when the child prompts the user or when live progress should be visible.

```rust
use procpilot::{Cmd, Redirection};

Cmd::new("cargo")
    .args(["build", "--release"])
    .stderr(Redirection::Inherit)
    .run()?;
# Ok::<(), procpilot::RunError>(())
```

### Stdin

```rust
use procpilot::Cmd;

let manifest = "apiVersion: v1\nkind: ConfigMap\n...";
Cmd::new("kubectl").args(["apply", "-f", "-"]).stdin(manifest).run()?;
# Ok::<(), procpilot::RunError>(())
```

### Pipelines

Chain commands with [`Cmd::pipe`] or the `|` operator. Per-stage builders (`arg`, `args`, `env`, `in_dir`) target the rightmost stage; pipeline-level knobs (`stdin`, `timeout`, `retry`, `stderr`) apply to the pipeline.

```rust
use procpilot::Cmd;

let out = Cmd::new("git").args(["log", "--oneline"])
    .pipe(Cmd::new("grep").arg("feat"))
    .pipe(Cmd::new("head").arg("-5"))
    .run()?;

// Same, with `|`:
let out = (Cmd::new("git").args(["log", "--oneline"])
    | Cmd::new("grep").arg("feat")
    | Cmd::new("head").arg("-5"))
    .run()?;
# Ok::<(), procpilot::RunError>(())
```

Failure status follows pipefail semantics: any non-success trumps success; the **rightmost** non-success wins. All stages' stderr is captured and concatenated (capture mode) or routed identically (inherit/null/file).

### Streaming (spawned processes)

For long-lived or bidirectional processes, use [`Cmd::spawn`] instead of `.run()`. `SpawnedProcess` exposes ownership of stdin/stdout pipes; lifecycle methods (`wait`, `kill`) take `&self` so the handle can be shared across threads.

```rust
use std::io::{BufRead, BufReader, Write};
use std::thread;
use procpilot::Cmd;

// `git cat-file --batch` pattern: write requests on one thread, read
// responses on another.
let proc = Cmd::new("git")
    .args(["cat-file", "--batch"])
    .in_dir("/repo")
    .spawn()?;

let mut stdin = proc.take_stdin().expect("piped");
let stdout = proc.take_stdout().expect("piped");

thread::spawn(move || {
    writeln!(stdin, "HEAD").ok();
    // drop(stdin) sends EOF
});

let mut reader = BufReader::new(stdout);
let mut header = String::new();
reader.read_line(&mut header)?;
// ... parse headers + binary content ...

let _ = proc.wait();
# Ok::<(), Box<dyn std::error::Error>>(())
```

Line-at-a-time variant:

```rust
use procpilot::Cmd;

Cmd::new("cargo")
    .args(["check", "--message-format=json"])
    .spawn_and_collect_lines(|line| {
        // e.g., serde_json::from_str(line)?;
        Ok(())
    })?;
# Ok::<(), procpilot::RunError>(())
```

### Secret redaction

```rust
use procpilot::Cmd;
Cmd::new("docker").args(["login", "-p", "hunter2"]).secret().run()?;
// Error messages show `docker <secret>` instead of the token.
# Ok::<(), procpilot::RunError>(())
```

## Async (tokio)

Enable the `tokio` feature to use `.run_async()` and `.spawn_async()` from inside a tokio runtime. The sync `.run()` would block the executor thread.

```toml
[dependencies]
procpilot = { version = "0.6", features = ["tokio"] }
```

```rust
use procpilot::Cmd;

let out = Cmd::new("git")
    .args(["rev-parse", "HEAD"])
    .in_dir(&repo)
    .run_async()
    .await?;
# Ok::<(), procpilot::RunError>(())
```

Run commands concurrently:

```rust
use procpilot::Cmd;

let branch = Cmd::new("git").args(["branch", "--show-current"]).in_dir(&repo).run_async();
let remote = Cmd::new("git").args(["remote", "get-url", "origin"]).in_dir(&repo).run_async();
let status = Cmd::new("git").args(["status", "--porcelain"]).in_dir(&repo).run_async();

let (b, r, s) = tokio::try_join!(branch, remote, status)?;
# Ok::<(), procpilot::RunError>(())
```

`.spawn_async()` returns an `AsyncSpawnedProcess` for streaming:

```rust
use procpilot::Cmd;
use tokio::io::{AsyncBufReadExt, BufReader};

let mut proc = Cmd::new("kubectl").args(["logs", "-f", pod]).spawn_async().await?;
let stdout = proc.take_stdout().expect("piped");
let mut lines = BufReader::new(stdout).lines();
while let Ok(Some(line)) = lines.next_line().await {
    handle(&line);
}
proc.wait().await?;
# Ok::<(), procpilot::RunError>(())
```

Cancellation via `tokio::select!`:

```rust
tokio::select! {
    res = proc.wait() => { res?; }
    _ = cancel.cancelled() => {
        let _ = proc.kill().await;
        let _ = proc.wait().await;
    }
}
# Ok::<(), procpilot::RunError>(())
```

Pipelines:

```rust
let out = (Cmd::new("git").args(["log", "--oneline"]) | Cmd::new("head").arg("-5"))
    .run_async()
    .await?;
# Ok::<(), procpilot::RunError>(())
```

All builder knobs (`arg`, `args`, `env`, `envs`, `in_dir`, `stdin`, `stderr`, `timeout`, `deadline`, `retry`, `retry_when`, `secret`, `pipe`, `|`) work identically on the async path.

Not yet on the async path:
- `impl AsyncRead for AsyncSpawnedProcess` (use `take_stdout()`).
- `&self` lifecycle methods — use `tokio::select!` to race `wait` against `kill`.

## License

Licensed under either [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT) at your option.
