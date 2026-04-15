# Changelog

All notable changes to procpilot are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
