# TODO

Ratchets the owner's quality rules ask for that are not done yet. Each entry drops in the change
that finishes it.

- **Property tests (owed in `tests/properties.toml`):** `src/cmd_display.rs` (roundtrip through
  `shlex`, as the fuzz target asserts), `src/error.rs` (predicates and the output-tail rule) and
  `src/retry.rs` (`default_transient`).
- **Lint suppressions:** `src/cmd/async_cmd.rs` carries `#[allow(clippy::too_many_arguments)]`
  on `run_one_attempt_async`; an options struct removes it and lowers the pin in
  `tests/lint_suppressions.rs`. The two `dead_code` allows in `tests/prelude.rs` go the same way.
  `clippy.toml` thresholds above clippy's defaults are pinned too (arguments 10, complexity 30,
  function lines 275).
- **File length:** `src/cmd.rs` (1779), `src/cmd/async_cmd.rs` (810) and `src/testing.rs` (491)
  are pinned over 400 and may only shrink.
- **Mutation testing:** a push made only of `chore(deps)` / `build(deps)` commits should skip the
  mutation jobs (a first job reads the push's commit subjects); `mutants.yml` has no such job.
- **Fuzzing:** the PATH search's `#!` line parser in `src/cmd/program.rs` is not fuzzed.
- **Marketing ratchets:** no `marketing/positioning.toml`, VHS demo tape or dated comparison page.
  Every text is the owner's to approve.
- **Docs:** the README is the only docs. A Getting Started path, a "For agents" snippet and
  `llms.txt` are owed; new prose needs the owner's approval of before and after.
- **Not applicable:** per-frame work, terminal UI, MCP server, blind evaluation, parsers of
  another program's output (the crate runs programs; it parses none of their formats).
