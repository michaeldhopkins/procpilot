//! The repo's CI and toolchain rules, as a test: a workflow is YAML no compiler reads, so
//! drift (a job without a timeout, a `cargo deny` limited to licenses) is silent otherwise.

use std::path::{Path, PathBuf};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(rel: &str) -> String {
    std::fs::read_to_string(root().join(rel)).unwrap_or_else(|e| panic!("read {rel}: {e}"))
}

fn workflows() -> Vec<(String, String)> {
    let dir = root().join(".github/workflows");
    let mut out: Vec<_> = std::fs::read_dir(&dir)
        .expect("read .github/workflows")
        .map(|e| e.expect("dir entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "yml"))
        .map(|p| {
            (
                name_of(&p),
                std::fs::read_to_string(&p).expect("read workflow"),
            )
        })
        .collect();
    out.sort();
    out
}

fn name_of(p: &Path) -> String {
    p.file_name()
        .expect("file name")
        .to_string_lossy()
        .into_owned()
}

/// Job ids: the two-space-indented keys directly under `jobs:`.
fn job_count(text: &str) -> usize {
    let mut in_jobs = false;
    let mut n = 0;
    for line in text.lines() {
        if line.starts_with("jobs:") {
            in_jobs = true;
        } else if in_jobs && !line.starts_with(' ') && !line.is_empty() && !line.starts_with('#') {
            in_jobs = false;
        } else if in_jobs && line.starts_with("  ") && !line.starts_with("   ") {
            let t = line.trim();
            if t.ends_with(':') && !t.starts_with('#') {
                n += 1;
            }
        }
    }
    n
}

fn has_top_level(text: &str, key: &str) -> bool {
    text.lines().any(|l| l.starts_with(key))
}

/// `channel = "1.99.0"` style: three numeric components, nothing floating.
fn is_exact_version(channel: &str) -> bool {
    let parts: Vec<_> = channel.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
}

#[test]
fn every_workflow_sets_permissions_and_concurrency() {
    let all = workflows();
    assert!(!all.is_empty(), "found no workflows");
    for (name, text) in &all {
        assert!(
            has_top_level(text, "permissions:"),
            "{name}: no top-level permissions"
        );
        assert!(
            has_top_level(text, "concurrency:"),
            "{name}: no top-level concurrency"
        );
    }
}

#[test]
fn every_job_sets_a_timeout() {
    for (name, text) in &workflows() {
        let jobs = job_count(text);
        assert!(jobs > 0, "{name}: found no jobs");
        let timeouts = text
            .lines()
            .filter(|l| l.trim_start().starts_with("timeout-minutes:"))
            .count();
        assert_eq!(
            timeouts, jobs,
            "{name}: {jobs} jobs but {timeouts} timeout-minutes"
        );
    }
}

#[test]
fn cargo_deny_is_never_limited_to_licenses() {
    for (name, text) in &workflows() {
        for line in text.lines().filter(|l| l.contains("cargo deny")) {
            assert!(
                !line.contains("check licenses"),
                "{name}: `{}` skips advisories",
                line.trim()
            );
        }
    }
    assert!(
        read("deny.toml").contains("[advisories]"),
        "deny.toml has no [advisories]"
    );
}

#[test]
fn ci_checks_formatting_docs_and_the_msrv() {
    let ci = read(".github/workflows/ci.yml");
    assert!(
        ci.contains("cargo fmt --all --check"),
        "ci.yml does not check formatting"
    );
    assert!(
        ci.contains("RUSTDOCFLAGS: \"-D warnings\""),
        "ci.yml does not deny doc warnings"
    );
    assert!(
        ci.contains("rust-version"),
        "ci.yml has no MSRV job reading rust-version"
    );
    assert!(
        ci.contains("run: cargo machete"),
        "ci.yml does not look for unused dependencies"
    );
}

#[test]
fn toolchain_is_pinned_exactly_with_clippy_and_rustfmt() {
    let toolchain = read("rust-toolchain.toml");
    let channel = toolchain
        .lines()
        .find_map(|l| l.trim().strip_prefix("channel = "))
        .expect("rust-toolchain.toml has a channel")
        .trim_matches('"');
    assert!(
        is_exact_version(channel),
        "channel {channel:?} is not an exact version"
    );
    assert!(toolchain.contains("clippy") && toolchain.contains("rustfmt"));
    assert!(root().join("rustfmt.toml").exists(), "no rustfmt.toml");
}

#[test]
fn exact_version_verdict() {
    assert!(is_exact_version("1.99.0"));
    for bad in ["stable", "1.99", "1.99.x", "1..0", "nightly-2026-01-01", ""] {
        assert!(!is_exact_version(bad), "{bad:?}");
    }
}

#[test]
fn job_count_reads_the_jobs_table_only() {
    let text = "name: x\non:\n  push:\njobs:\n  a:\n    steps:\n      - run: x\n  b:\n    name: B\nenv:\n  K: v\n";
    assert_eq!(job_count(text), 2);
}
