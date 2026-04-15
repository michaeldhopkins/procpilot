# Changelog

All notable changes to procpilot are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.4.0] - 2026-04-14

### Features

- **Pipelines.** [`Cmd::pipe`] and `impl BitOr` let you build N-ary pipelines: `Cmd::new("a") | Cmd::new("b") | Cmd::new("c")`. Pipelines execute with duct-style pipefail semantics — any non-success trumps success, and the rightmost non-success wins.
- Per-stage builder methods (`arg`, `args`, `env`, `envs`, `env_remove`, `env_clear`, `in_dir`) target the rightmost stage after `.pipe()`, so you can incrementally build each stage's configuration.
- Pipeline-level knobs (`stdin`, `stderr`, `timeout`, `deadline`, `retry`, `retry_when`, `secret`, `before_spawn`) apply to the whole pipeline.
- `CmdDisplay` now renders multi-stage pipelines shell-style (`a | b | c`) and respects `secret` on every stage.

### Implementation

- Uses [`os_pipe`](https://crates.io/crates/os_pipe) 1.2 for clean pipe fd management between stages.

### Limitations

- `Cmd::spawn` on a pipeline returns `RunError::Spawn` with `io::ErrorKind::Unsupported` — pipeline `SpawnedProcess` will land in a later release. Use `.run()` for pipelines.

## [0.3.0] - 2026-04-14

### Features

- **`Cmd::spawn`** — new entry point returning a [`SpawnedProcess`] handle for long-lived or bidirectional processes (`git cat-file --batch`, `kubectl logs -f`, `cargo check --message-format=json`). Covers the main use case that previously forced callers back to raw `std::process::Command`.
- `SpawnedProcess` methods: `take_stdin` / `take_stdout` (one-shot ownership of the pipes), `pids`, `kill`, `try_wait`, `wait`, `wait_timeout`. Lifecycle methods take `&self` so the handle shares cleanly across threads.
- Dual `Read` impls (`impl Read for SpawnedProcess` and `impl Read for &SpawnedProcess`) — read stdout through the handle; reference impl lets one thread read while another calls `kill`.
- `Cmd::spawn_and_collect_lines` — high-level helper for the line-streaming case; runs a `FnMut(&str) -> io::Result<()>` per line and returns the final `RunOutput`.
- Stderr (when `Redirection::Capture`) is drained into a background thread and attached to the `RunOutput` / `RunError` when `wait` resolves.

### Implementation

- Uses [`shared_child`](https://crates.io/crates/shared_child) 1.1 for lock-free concurrent kill-while-waiting.

## [0.2.0] - 2026-04-12

### Breaking changes

- **Free functions removed.** `run_cmd`, `run_cmd_in`, `run_cmd_in_with_env`, `run_cmd_in_with_timeout`, `run_cmd_inherited`, and `run_with_retry` have been removed in favor of the single [`Cmd`] builder.
- **`RunError` shape changed.** Variants now carry a `CmdDisplay` in the `command` field instead of `program: String` + `args: Vec<String>`. Stdout/stderr fields on `NonZeroExit` and `Timeout` are truncated to the last 128 KiB (`STREAM_SUFFIX_SIZE`).
- Migration: replace `run_cmd_in_with_env(&dir, prog, args, env)` with `Cmd::new(prog).args(args).in_dir(&dir).envs(env).run()`. Error field access changes from `{ program, args }` to `{ command }`; `err.program()` still works.

### Features

- New [`Cmd`] builder covers every knob: args, cwd, env, stdin, stderr routing, timeout, deadline, retry, secret redaction, `before_spawn` hook, and `to_command` escape hatch.
- Stdin piping via [`Cmd::stdin`] — accepts `Vec<u8>`, `&[u8]`, `String`, `&str`, or a boxed `Read` via [`StdinData::from_reader`].
- Stderr routing via [`Redirection`] (`Capture` / `Inherit` / `Null` / `File`). Marked `#[non_exhaustive]` so future variants (e.g., `Merge` for `2>&1`) can land without another breaking change.
- [`RetryPolicy`] wraps `backon`'s `ExponentialBuilder` with a default predicate retrying on `"stale"` / `".lock"` stderr.
- [`Cmd::deadline`] for overall wall-clock budget that composes across retries.
- [`Cmd::secret`] redacts args as `<secret>` in [`CmdDisplay`] and error formatting.
- [`Cmd::before_spawn`] hook for `pre_exec`, umask, and other Unix escape hatches.
- [`Cmd::to_command`] drops to raw `std::process::Command` for cases the builder doesn't cover.

## [0.1.1] - 2026-04-14

### Miscellaneous

- Add project quality apparatus: `clippy.toml`, `cliff.toml`, `CLAUDE.md`, `scripts/stats.sh`, `examples/basic.rs`
- Add mock test binaries in `src/bin/pp_*` gated behind the `test-helpers` feature so they don't install via `cargo install`
- Set up `[package.metadata.docs.rs]` for clean feature-gated docs

## [0.1.0] - 2026-04-14

### Features

- Initial release: production-grade subprocess runner with typed errors, retry, and timeout
- `RunError` enum distinguishing `Spawn` / `NonZeroExit` / `Timeout`, marked `#[non_exhaustive]`
- Retry with exponential backoff via `backon`
- Timeout with pipe-draining background threads to prevent deadlock on chatty processes
- Binary-safe `Vec<u8>` stdout plus `stdout_lossy()` convenience
- Env var support via `run_cmd_in_with_env`
- `binary_available` / `binary_version` helpers
