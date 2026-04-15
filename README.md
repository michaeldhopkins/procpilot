# procpilot

Production-grade subprocess runner for Rust — typed errors, retry with backoff, timeout with pipe-draining, binary-safe output.

Built for CLI tools that need to spawn external processes and handle failure modes precisely. Not intended for shell scripting (see [xshell](https://crates.io/crates/xshell) for that).

## Why not `std::process::Command`?

`Command::output()` returns an `Output` with a status field the caller must remember to check. `Command::spawn()` gives you a `Child` but no help with timeout, retry, or deadlock-safe pipe draining. Building a production CLI tool on `Command` alone means writing the same wrapping layer every time.

`procpilot` is that layer:

- **Typed errors** — [`RunError`] distinguishes `Spawn` (couldn't start — binary missing, fork failed), `NonZeroExit` (command ran and reported failure with captured stdout/stderr), and `Timeout` (killed after exceeding budget). Callers can match to handle each appropriately.
- **Retry with exponential backoff** — [`run_with_retry`] wraps any subprocess call with a user-supplied "is this error transient?" predicate.
- **Timeout with pipe-draining** — background threads drain stdout/stderr while waiting, so chatty processes don't block on buffer overflow and fail to respond to the kill signal.
- **Binary-safe output** — stdout is `Vec<u8>` (faithful for image/binary content); `stdout_lossy()` gives a zero-copy `Cow<str>` for text.
- **Environment variables** — [`run_cmd_in_with_env`] handles the `GIT_INDEX_FILE` / `SSH_AUTH_SOCK` / etc. cases without dropping back to `Command`.

## Usage

```toml
[dependencies]
procpilot = "0.1"
```

### Running commands

```rust
use procpilot::{run_cmd, run_cmd_in, RunError};

// Basic: run a command, get captured output
let output = run_cmd("echo", &["hello"])?;
assert_eq!(output.stdout_lossy().trim(), "hello");

// In a specific directory
let output = run_cmd_in(&repo_path, "git", &["log", "--oneline", "-5"])?;

// Binary-safe output for image/binary content
let output = run_cmd_in(&repo_path, "git", &["show", "HEAD:logo.png"])?;
let image_bytes: Vec<u8> = output.stdout;
```

### Handling "command ran and said no"

`procpilot` returns `Result<RunOutput, RunError>`. The three-variant enum lets callers distinguish infrastructure failure from command-reported failure from timeouts:

```rust
use procpilot::{run_cmd, RunError};

match run_cmd("git", &["show", "maybe-missing-ref"]) {
    Ok(output) => Some(output.stdout),
    Err(RunError::NonZeroExit { .. }) => None,   // ref doesn't exist — legitimate answer
    Err(e) => return Err(e.into()),              // real infrastructure failure
}
# ; Ok::<(), anyhow::Error>(())
```

`RunError` implements `std::error::Error`, so `?` into `anyhow::Result` works when you don't care about the distinction.

Inspection methods on `RunError`:
- `err.is_non_zero_exit()` / `err.is_spawn_failure()` / `err.is_timeout()` — check the variant
- `err.stderr()` — captured stderr on `NonZeroExit`/`Timeout`, `None` on `Spawn`
- `err.exit_status()` — exit status on `NonZeroExit`, `None` on others
- `err.program()` — the program name that failed

`RunError` is marked `#[non_exhaustive]`, so future variants won't break your match arms — include a wildcard fallback.

### Timeouts

For commands that might hang (network operations, unreachable remotes, user-supplied queries), use the timeout variant:

```rust
use std::time::Duration;
use procpilot::{run_cmd_in_with_timeout, RunError};

match run_cmd_in_with_timeout(&repo, "git", &["fetch"], Duration::from_secs(30)) {
    Ok(_) => println!("fetched"),
    Err(RunError::Timeout { elapsed, stderr, .. }) => {
        eprintln!("fetch hung after {elapsed:?}; last stderr: {stderr}");
    }
    Err(e) => return Err(e.into()),
}
# ; Ok::<(), anyhow::Error>(())
```

Output collected before the kill is returned in the `Timeout` error variant.

**Caveat on grandchildren:** the kill signal reaches only the direct child. A shell wrapper like `sh -c "slow-cmd"` forks the target as a grandchild that survives the shell's kill. Use `exec` in the shell (`sh -c "exec slow-cmd"`) or invoke the target directly.

### Retry on transient errors

```rust
use procpilot::{run_with_retry, RunError};

// Retry when stderr looks like a lock-contention error
let output = run_with_retry(&repo, "git", &["pull"], |err| match err {
    RunError::NonZeroExit { stderr, .. } => stderr.contains(".lock"),
    _ => false,
})?;
# ; Ok::<(), RunError>(())
```

Uses exponential backoff (100ms, 200ms, 400ms) with up to 3 retries. The predicate is `impl Fn(&RunError) -> bool` so it can capture state.

### Environment variables

```rust
use procpilot::run_cmd_in_with_env;

// Run with a custom GIT_INDEX_FILE (detects unstaged renames)
let output = run_cmd_in_with_env(
    &repo, "git", &["add", "-N", "--", "file.rs"],
    &[("GIT_INDEX_FILE", "/tmp/index.tmp")],
)?;
# ; Ok::<(), procpilot::RunError>(())
```

### Inherited I/O

When the user should see output directly (e.g., running `cargo test` and letting test output stream):

```rust
use procpilot::run_cmd_inherited;

run_cmd_inherited("cargo", &["test"])?;
# ; Ok::<(), procpilot::RunError>(())
```

### Binary availability

```rust
use procpilot::{binary_available, binary_version};

if binary_available("docker") {
    println!("docker version: {}", binary_version("docker").unwrap_or_default());
}
```

## License

Licensed under either [Apache-2.0](LICENSE-APACHE) or [MIT](LICENSE-MIT) at your option.
