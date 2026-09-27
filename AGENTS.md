# procpilot

Production-grade subprocess runner for Rust. Published as a library crate on crates.io. Primary consumer is vcs-runner; secondary consumers are production CLI tools (cargo subcommands, devops wrappers, etc.) that need typed errors, retry, and timeout for their subprocess calls.

## Pre-commit checklist

Before every commit, verify:
1. [ ] `cargo clippy --all-features --all-targets -- -D warnings` passes
2. [ ] `cargo clippy --no-default-features --all-targets -- -D warnings` passes
3. [ ] `cargo test --all-features` passes (integration tests require `mock-binaries`)
4. [ ] `cargo test --lib` passes without features (unit-only sanity check)
5. [ ] `cargo test --doc --all-features` passes
6. [ ] Version bumped in `Cargo.toml` (patch for fixes/docs, minor for features, `0.x` breaking bumps minor)
7. [ ] `cargo check` run after version bump (updates `Cargo.lock`)
8. [ ] If the release will generate user-visible changes, `git cliff --output CHANGELOG.md`
9. [ ] No file under `src/` over 400 production lines (`tests/file_length.rs`; inline test modules are not counted). Files already over when the gate went in (2026-09-14) are pinned at that size and may only shrink. New code goes in a new module, never into a pinned file.

Never use `#[allow(...)]` to suppress warnings — fix the underlying issue.

## Antipatterns (not caught by lints)

**Stringly-typed error handling.** Don't use `.contains()` on stderr strings to branch logic. Use the typed `RunError` variants. Match on structure.

**Panics in error handlers.** `panic!()` inside closures is user-hostile. For library code, return `Result`. `.expect("reason")` is acceptable only for invariants that are truly impossible to violate at runtime.

**Unnecessary cloning.** Watch for `.clone().or(.clone())`. Watch for functions taking `&T` that internally clone. Cloning `Arc`s and `PathBuf`s for thread/retry use is correct.

**Code duplication.** If the same logic appears 3+ times, extract a helper. Lints don't catch semantic duplication.

**Local fixes that ignore root cause.** Adding `.clone()` to satisfy the borrow checker instead of restructuring. Wrapping errors in strings instead of adding enum variants. Suppressing warnings instead of fixing the underlying issue.

**Feature-gate leaks.** Items gated behind a feature should not be visible in docs when the feature is off (`#[cfg_attr(docsrs, doc(cfg(feature = "...")))]`).

## Testing requirements

**Every behavioral change requires tests.** This is non-negotiable.

- New functions/methods: unit tests covering happy path + at least one edge case (failure, empty input, unusual args)
- Bug fixes: write a failing test first, then fix
- Refactors: existing tests must pass; add tests if coverage gaps surface

If testing is hard, that's a signal the code needs refactoring. Extract pure functions, introduce seams.

### Mock binaries pattern

Tests use small Rust mock binaries in `src/bin/pp_*.rs`, not `/bin/sh` or shell builtins. This is portable across macOS/Linux/Windows and doesn't depend on environment shells behaving identically. Reference them via `env!("CARGO_BIN_EXE_pp_echo")` etc. in tests.

When a test needs a new subprocess behavior that existing mocks don't provide, add a new mock binary rather than resorting to `sh -c "..."`. See `src/bin/pp_cat.rs` etc. for the pattern.

## Semver (0.x conventions)

**PATCH (0.x.Y → 0.x.Y+1):** bug fixes, docs, internal refactor, dep updates, test additions, non-breaking doc-comment changes.

**MINOR (0.X.y → 0.X+1.0):** new public APIs, new features, or **any breaking change** (standard semver 0.x convention — minor bumps are breaking during 0.x).

For breaking releases, document migration steps in the commit message and release notes. Create a `MIGRATION.md` if cumulative breakage gets complex enough to need a dedicated guide.

## CI expectations

CI runs on push/PR:
- `cargo check --locked`
- `cargo check --locked --no-default-features`
- `cargo test --locked`
- `cargo test --locked --no-default-features`
- `cargo clippy --locked --all-targets -- -D warnings`
- `cargo clippy --locked --no-default-features --all-targets -- -D warnings`
- `cargo doc --no-deps` with `RUSTDOCFLAGS="-D warnings"` (catches broken doc links)
- `cargo deny check licenses`

Fuzzing has its own workflows; see "Fuzzing" below. So does mutation testing; see "Mutation testing".

Release workflow publishes to crates.io on version-bump push to main.

## Architecture notes

- `src/lib.rs` re-exports the public API. Nothing lives directly here except the crate-level doc and module declarations.
- `src/error.rs` — `RunError` enum
- `src/runner.rs` — free function API (to be replaced by `Cmd` builder in 0.2.0)
- `src/bin/pp_*.rs` — mock test binaries (not public API; used internally)

Once Phase 1 (0.2.0) lands, the module shape becomes:
- `src/cmd.rs` — `Cmd` builder (primary API)
- `src/error.rs` — `RunError`, `CmdDisplay`
- `src/stdin.rs`, `src/redirection.rs`, `src/retry.rs` — supporting types
- `src/spawned.rs` (Phase 2) — `SpawnedProcess`

## Fuzzing

cargo-fuzz targets live in `fuzz/` (a standalone workspace, nightly only). The method is the `rust-fuzzing` skill; this section is what is true only of procpilot. The crate builds no shipped binary (the `pp_*` bins are test mocks behind `mock-binaries`), so what is fuzzed is the library's own surface that takes input nobody here wrote.

| Target | Input | Asserts |
|---|---|---|
| `cmd_display` | programs and arguments a caller hands `Cmd` (arbitrary bytes, via `arbitrary`), pipelines, `.secret()` | `CmdDisplay` split back by `shlex` (an independent POSIX splitter) gives the same argv, with `\|` between stages; secret renders split to exactly `program <secret>` per stage; outside single quotes the render holds no character a shell expands (`$`, `*`, `~`, ...), which `shlex` cannot see since it does no expansion; a program left unquoted is never an assignment (`f=`) or a reserved word (`if`) |
| `captured_output` | a child's stdout/stderr bytes (invalid UTF-8, split characters) | through `src/fuzz_api.rs`: stdout keeps exactly the last `STREAM_SUFFIX_SIZE` bytes; stderr, lossily decoded, is a tail of the decoded text within 3 bytes under the cap |

- `src/fuzz_api.rs` exists only under `--cfg fuzzing` and reaches crate-private functions without publishing them.
- `captured_output` pads the fuzzed bytes with 128 KiB of ASCII so the cut lands inside them; its first version put the padding in front, so the cut always fell in the padding and a planted bug in the character-boundary walk stayed green.
- Words that are not UTF-8 are rendered lossily by design, so `cmd_display` checks only that they do not panic.
- Seeds (`fuzz/corpus/<t>/seed-*`) are the inputs that turned each planted mutation red, so the replay alone catches those regressions. `seed-found-program-assignment` is the input that found the unquoted `f=` program.
- `fuzz/dict/cmd_display.dict` is shell metacharacters, the reserved-word probe and a split UTF-8 character.
- Not fuzzed yet: the PATH search's `#!` line parser (`format_of` / `interpreter` in `src/cmd/program.rs`) reads the head of whatever file sits on `PATH`, which is a real surface. It was left alone while that module was being rewritten; it is the next target to add, as a never-panics target over the head bytes (a `Runnable` answer also needs the named interpreter to exist, so keep the file checks out of it).
- Not fuzzed, and why: retry schedules are backon's, and procpilot only passes the builder through; spawning, piping and timeouts are covered by the integration tests with the `pp_*` mocks, where the bytes do not change the code path.

CI: `fuzz-replay.yml` replays the corpus on every push to `main` and every PR (the gate); `fuzz.yml` bursts each target for ~180s on each push to `main` through `fuzz/burst.sh` and saves the merged corpus to the Actions cache (not a gate; `workflow_dispatch` takes a longer budget). No schedule. `tests/fuzz_targets_wired.rs` fails when `fuzz/Cargo.toml` and the workflows disagree.

Locally, one target at a time:

```sh
cargo +nightly fuzz build cmd_display
fuzz/burst.sh fuzz/target/aarch64-apple-darwin/release/cmd_display cmd_display 60
```

## Mutation testing

cargo-mutants, per the `rust-mutation-testing` skill; this section is only what is true of procpilot. `.cargo/mutants.toml` makes a bare `cargo mutants` work: it turns on all features (the integration tests need `mock-binaries`, `tokio` and `testing`), leaves out the `pp_*` mock binaries (test fixtures, never shipped) and `src/fuzz_api.rs` (compiled only under `cargo fuzz`), and excludes six equivalent or timing-only mutants (four patterns), each with its reason.

**CI** (`.github/workflows/mutants.yml`, not a gate): `--in-diff` on every PR and push to main, and on pushes to main one rotating slice, `--shard (run_number % 4)/4`. Both run `-j2` with stdin closed (see below). No whole-tree run and no schedule.

**Adopted 2026-09-27.** 481 mutants after exclusions (`cargo mutants --list | wc -l`). No whole-tree sweep: slices 0/4 and 1/4 were run and burned down; 2/4 (error, spawned, testing, stdin, retry, runner) and 3/4 (`src/cmd/async_cmd.rs`, `src/cmd/program.rs`) will be covered by CI's rotation.

- **N = 4.** Completed slices at `-j2` on an M3 with other builds running: 0/4 in 2m46s, 1/4 in 3m32s (121 mutants each, baseline 4-5s build + 3-4s test). A hosted runner is slower, so a slice should land near 10 minutes, inside the 20-minute job timeout. An earlier run of slice 0/4 took 15 minutes; contention and the stdin hang below were most of that. Re-measure from the first CI runs and adjust `SLICES` if a slice goes past 15 minutes.
- **Score**, caught / (caught + missed): the two slices went from 130 / 153 (85%) to 153 / 158 (97%). The 5 still missed are the equivalents below, so every killable mutant in those slices is caught. Before the burn-down: 23 missed and 2 timeouts across the two slices, plus 1 miss in a `src/cmd_display.rs` pilot.
- **What the misses were:** `AsyncSpawnedProcess` had no test for `is_pipeline` on a single command, `try_wait` or `wait_timeout` returning an output, captured stderr, `Debug`, `kill`, or pipefail. The sync cancel path had no test for the grace period (`pp_sleep --ignore-sigterm` exists for that), for cancel combined with a timeout, or for backoff still sleeping when a cancel flag is set. Also missing: a deadline checked by the retry loop, stdin on a spawned pipeline, the negative forms of the `RunError` predicates and `is_secret`, and `STREAM_SUFFIX_SIZE`'s value.
- **Two misses that looked caught.** `AsyncSpawnedProcess::kill -> Ok(())` and `pipefail_status` were caught in one run and missed in the next with no code change. Some unrelated test failed by chance under load. Each now has a test that asserts the behaviour itself.
- **Timeouts became catches.** The cancel tests slept 60s, so a broken cancel check hung the suite until cargo-mutants' ~20s timeout. At 8s, the same mutant fails the test when the sleep ends. `pipeline_does_not_deadlock_on_large_output` now takes a timeout for the same reason. Keep new cancel and deadlock tests under ~10s of worst-case sleep.

**Known equivalents left MISSED, not excluded,** because `exclude_re` matches names, and these share theirs with mutants the tests do catch. A line-anchored pattern would drift with any edit above it.

- `replace - with + / with / in execute_pipeline` and `spawn_pipeline_stages` (and their async twins) at `for _ in 0..stages.len() - 1`: one extra close-on-exec pipe is opened and dropped. Writing the range as `1..stages.len()` removes these mutants. That is a source change, so it belongs in a release.
- `replace && with || in execute_pipeline` (and its async twin) at `if i == stages.len() - 1 && matches!(stdout_mode, Capture)`: `child.stdout` is `Some` only for a piped (Capture) last stage, so the extra `take()` calls return `None`.

**Stdin must be closed for local runs.** `run_async_async_reader_is_one_shot_across_clones` runs a second clone whose one-shot reader is already taken. That clone then inherits the test process's stdin, and its `pp_cat` reads to EOF. If that stdin stays open and silent, EOF never comes and the suite hangs at 0% CPU: seen twice on 2026-09-27 from an agent's shell, whose stdin is a socket (45 other runs from the same shell passed). With stdin at `/dev/null` the second clone reads nothing, as the test expects. Run `cargo mutants ... < /dev/null` (CI does). Whether a consumed reader should give the child an empty stdin rather than the parent's is an open question for the library; the test only encodes the current behaviour.
