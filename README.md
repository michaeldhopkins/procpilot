# procpilot

Production-grade subprocess runner for Rust — typed errors, retry with backoff, timeout, stdin piping, secret redaction.

Built for CLI tools that spawn external processes and need precise failure handling. Not intended for shell scripting (see [xshell](https://crates.io/crates/xshell) for that).

## Why not `std::process::Command`?

`Command::output()` returns a status the caller must remember to check. `Command::spawn()` gives you a `Child` but no help with timeout, retry, or deadlock-safe pipe draining. Every production CLI ends up writing the same wrapping layer.

`procpilot` is that layer.

- **Typed errors** — [`RunError`] distinguishes `Spawn` (couldn't start), `NonZeroExit` (command ran and failed, with captured stdout/stderr), and `Timeout` (killed after budget).
- **Retry with exponential backoff + jitter** — [`Cmd::retry`] / [`Cmd::retry_when`].
- **Timeout + deadline** — per-attempt timeout or overall wall-clock budget across retries.
- **Stdin piping** — owned bytes (reusable across retries) or a boxed `Read` (one-shot streaming).
- **Stderr routing** — capture / inherit / null / redirect-to-file via [`Redirection`].
- **Secret redaction** — [`Cmd::secret`] replaces args with `<secret>` in error output and logs.
- **Streaming / bidirectional** — [`Cmd::spawn`] returns a [`SpawnedProcess`] (single command or pipeline) with `take_stdin` / `take_stdout`, `Read` impls, `kill`, `wait`, `wait_timeout`, and `spawn_and_collect_lines` for line-by-line callbacks.
- **Pipelines** — [`Cmd::pipe`] or the `|` operator chains commands (`a | b | c`) with duct-style pipefail status precedence. `Cmd::run()` and `Cmd::spawn()` both work on pipelines.
- **Cloneable `Cmd`** — configure a base `Cmd` once, clone it to branch off variants. Bytes-stdin and file handles are `Arc`-shared across clones; reader-stdin is one-shot.
- **`impl Display for Cmd`** — `format!("{cmd}")` renders shell-style with secret redaction.

## Usage

```toml
[dependencies]
procpilot = "0.5"
```

### Reusing a base `Cmd`

```rust
use procpilot::Cmd;

let base = Cmd::new("git").in_dir("/repo").env("GIT_TERMINAL_PROMPT", "0");
let status = base.clone().args(["status", "--short"]).run()?;
let log    = base.clone().args(["log", "-1", "--oneline"]).run()?;
# Ok::<(), procpilot::RunError>(())
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

`.timeout()` bounds a single attempt; `.deadline()` bounds the whole operation (including retry backoff sleeps). Combine them when you want "retry up to 3× but never exceed 10 seconds total".

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

### Inheriting stderr (live progress)

When the child prompts the user or should stream progress to the terminal, route stderr with `Redirection::Inherit` instead of capturing it.

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

Chain commands with [`Cmd::pipe`] or the `|` operator. Per-stage builders (`arg`, `args`, `env`, `in_dir`) target the rightmost stage; pipeline-level knobs (`stdin`, `timeout`, `retry`, `stderr`) apply to the whole thing.

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

Failure status follows duct's pipefail rule: any non-success trumps success; the **rightmost** non-success wins. All stages' stderr is captured and concatenated (capture mode) or routed identically (inherit/null/file).

### Streaming (spawned processes)

For long-lived or bidirectional processes, use [`Cmd::spawn`] instead of `.run()`. The returned `SpawnedProcess` gives you ownership of stdin/stdout pipes and `&self` `wait` / `kill` so you can share the handle across threads.

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

For the common "read lines as they arrive" case:

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

## License

Licensed under either [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT) at your option.
