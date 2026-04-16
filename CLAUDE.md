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
