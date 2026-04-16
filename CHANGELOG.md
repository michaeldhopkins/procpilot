# Changelog

All notable changes to procpilot are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.6.1] - 2026-04-15

### Features

- **`procpilot::prelude`** module exporting the everyday types: `Cmd`, `RunError`, `RunOutput`, `Redirection`, `RetryPolicy`, `StdinData`, `SpawnedProcess`, and (with the `tokio` feature) `AsyncSpawnedProcess`. `use procpilot::prelude::*;` cuts the import line for typical callers.

## [0.6.0] - 2026-04-15

### Breaking changes

- **`StdinData` is now `#[non_exhaustive]`.** Downstream `match` expressions on the enum require a wildcard arm.
- **`StdinData::Reader` variant's inner type is `Box<dyn Read + Send + 'static>`** (previously `+ Sync`). Migration: no code change unless you were relying on the `Sync` bound at the variant's pattern-bound `r`; callers that accept the type can loosen their bounds in turn.
- **`RunOutput` is now `#[non_exhaustive]`.** Downstream construction via struct literal is no longer allowed; use procpilot's runners to obtain one. Field access is unchanged.
- **`Cmd::to_command` renamed to [`Cmd::to_rightmost_command`].** The old name silently returned the rightmost stage for pipelines; the rename makes that explicit. For single commands the behavior is identical. For pipelines, use [`Cmd::to_commands`] to get every stage.
- **`Redirection::File(File)` (pre-0.5.0) stays `Redirection::File(Arc<File>)`**; construct via new [`Redirection::file`] or [`Cmd::stderr_file`] / [`Cmd::stdout_file`] helpers instead of wrapping `Arc` yourself.
- **`BeforeSpawnHook` type alias is no longer a public re-export.** Callers pass closures to [`Cmd::before_spawn`]; no type name needed.
- **`StdinData::is_reusable()` removed.** Match on the variant directly if you need this distinction (rare).

### Features

- **`Cmd::run_async` and `Cmd::spawn_async`** behind the new `tokio` feature (opt-in). Single commands and pipelines on both; `spawn_async` returns an `AsyncSpawnedProcess` handle.
- **`AsyncSpawnedProcess`** — tokio counterpart to `SpawnedProcess`. `take_stdin` / `take_stdout` (tokio types), `pids`, `kill`, `wait`, `try_wait`, `wait_timeout`. Pipeline support with pipefail status precedence.
- **`StdinData::AsyncReader` variant** + **`StdinData::from_async_reader`** constructor (tokio feature). True async streaming via `tokio::io::copy` — no buffering. Passing to the sync runner returns `RunError::Spawn` with `ErrorKind::InvalidInput`.
- **`before_spawn` works on the async path** via `tokio::process::Command::as_std_mut()`. Same hook signature; fires per stage per retry attempt on both sync and async paths.
- **Idempotent `wait` / `try_wait` / `wait_timeout`** on `SpawnedProcess` and `AsyncSpawnedProcess`. First finalize caches stdout / stderr / per-stage statuses in an internal `Arc`; subsequent calls reconstruct the same `Result`. Matters for `tokio::select!` cancellation patterns and for any retry-after-kill flow. Concurrent `wait` from multiple threads on sync `SpawnedProcess` is serialized via a mutex so no split-brain state.
- **Mid-pipeline spawn-failure cleanup.** When a later stage's spawn fails, already-spawned stages are killed before the error propagates — sync path uses explicit `kill + wait`, async path uses `tokio::process::Command::kill_on_drop(true)`.
- **Iterative `CmdTree` flatten.** Removes recursion risk on pathologically deep pipelines.
- **Stdout routing.** New [`Cmd::stdout`] builder accepting a [`Redirection`] plus [`Cmd::stdout_file`] / [`Cmd::stderr_file`] shortcuts and [`Redirection::file`] constructor. Honored on [`Cmd::run`] / [`Cmd::run_async`]; rejected (with `ErrorKind::InvalidInput`) on [`Cmd::spawn`] / [`Cmd::spawn_async`] because the handle needs stdout piped.

### Not yet on the async path

- `impl AsyncRead for AsyncSpawnedProcess` — use `take_stdout()` meanwhile.
- Concurrent `kill`-during-`wait` via `&self` — use `tokio::select!` to race wait against kill.

## [0.5.1] - 2026-04-14

### Features

- `impl fmt::Display for Cmd` — `format!("{cmd}")` now works directly, delegating to `CmdDisplay` (shell-quoted, secret-respecting).

### Docs

- Document that cloning a `Cmd` with a reader-based stdin produces a one-shot Mutex: whichever attempt runs first takes the reader, later clones/retries see no stdin. Bytes-based stdin is shared via `Arc` and re-feeds on every attempt.

### Tests

- Added coverage of the reader-stdin one-shot invariant across clones.

## [0.5.0] - 2026-04-14

### Breaking changes

- **`Redirection::File(File)` → `Redirection::File(Arc<File>)`.** Enables [`Cmd`] to be `Clone` without losing the file handle. Migration: wrap the `File` in `Arc::new(...)` at the call site.

### Features

- **`Cmd` now implements `Clone`.** Template-and-vary is now ergonomic: configure a base `Cmd`, clone to branch off variants. Internally, stdin data is `Arc`-shared — `Bytes` variants share the same buffer across clones (cheap), and `Reader` variants share a `Mutex<Option<…>>` so the first attempt to run takes the reader and subsequent clones or retries see no stdin. All other fields (program, args, envs, retry policy, before_spawn hook, stderr mode) clone cheaply via `Arc` or owned data.
- **`Cmd::to_commands()`** — returns one `std::process::Command` per pipeline stage, leftmost first. Complements `to_command()` (which for pipelines returns only the rightmost stage, now clearly documented).

### Docs

- `RunError::stderr` doc comment now calls out that the value is lossy-decoded UTF-8 — raw stderr bytes should be read via `Cmd::spawn`.

## [0.4.1] - 2026-04-14

### Features

- **`Cmd::spawn` now supports pipelines.** The returned [`SpawnedProcess`] holds every stage's `SharedChild`; `take_stdin` targets the leftmost stage, `take_stdout` the rightmost, and `kill` / `wait` / `try_wait` / `wait_timeout` operate on every stage. Status follows the same pipefail rule as `.run()`.
- `SpawnedProcess::is_pipeline()` and `pids()` (length > 1) expose the multi-stage shape.

### Internal

- Comment pass: removed dozens of "what" comments that restated the code; kept only "why" (non-obvious invariants, workarounds, intent of a test).

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
