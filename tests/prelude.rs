//! Verifies the `prelude` module exports the items advertised in its
//! doc-comment. Compiled as a downstream consumer would.

use procpilot::prelude::*;

#[allow(dead_code)]
fn _exports(
    _: Cmd,
    _: RunError,
    _: RunOutput,
    _: Redirection,
    _: RetryPolicy,
    _: StdinData,
    _: SpawnedProcess,
) {
}

#[cfg(feature = "tokio")]
#[allow(dead_code)]
fn _exports_async(_: AsyncSpawnedProcess) {}

#[test]
fn prelude_compiles() {
    // The compile-time references above are the real assertion. This test
    // exists so `cargo test` reports a passing entry for the file.
}
