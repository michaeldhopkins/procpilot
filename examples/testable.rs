//! How to write production code that's unit-testable without spawning
//! processes. Run with: `cargo run --example testable --features testing`.

use procpilot::{Cmd, RunError, Runner};
use std::path::Path;

/// Production helper. Takes `&dyn Runner` so it's testable with a mock.
pub fn current_branch(runner: &dyn Runner, repo: &Path) -> Result<String, RunError> {
    let cmd = Cmd::new("git")
        .args(["branch", "--show-current"])
        .in_dir(repo);
    let out = runner.run(cmd)?;
    Ok(out.stdout_lossy().trim().to_string())
}

fn main() {
    // In production, pass `&DefaultRunner` (real subprocess execution).
    use procpilot::DefaultRunner;
    match current_branch(&DefaultRunner, Path::new(".")) {
        Ok(branch) => println!("real branch: {branch}"),
        Err(e) => println!("not a git repo (expected if you're outside one): {e}"),
    }

    #[cfg(feature = "testing")]
    {
        // In tests, pass a `MockRunner` with canned outputs.
        use procpilot::testing::{MockRunner, ok_str};
        let mock = MockRunner::new().expect("git branch --show-current", ok_str("main\n"));
        let branch =
            current_branch(&mock, Path::new("/repo")).expect("mock returns canned output");
        println!("mocked branch: {branch}");
        mock.verify().expect("all expectations matched");
    }

    #[cfg(not(feature = "testing"))]
    {
        eprintln!("(re-run with --features testing to see the mock pattern)");
    }
}
