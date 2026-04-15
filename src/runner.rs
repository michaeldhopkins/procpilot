//! Binary-availability helpers that complement the [`Cmd`](crate::Cmd) builder.

use std::process::{Command, Stdio};

/// Check whether a binary is available on PATH.
///
/// Heuristic: spawns `name --version` and checks exit 0. Works for most Unix
/// tools (`git`, `jj`, `kubectl`, `docker`). Binaries that don't support
/// `--version` will appear unavailable even when present on PATH.
pub fn binary_available(name: &str) -> bool {
    Command::new(name)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// Return a binary's `--version` output, if available.
pub fn binary_version(name: &str) -> Option<String> {
    let output = Command::new(name).arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_available_missing_returns_false() {
        assert!(!binary_available("nonexistent_binary_xyz_42"));
    }

    #[test]
    fn binary_version_missing_returns_none() {
        assert!(binary_version("nonexistent_binary_xyz_42").is_none());
    }
}
