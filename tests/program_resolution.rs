//! A bare program name is resolved through `PATH` once, before the spawn.
//!
//! Left to the OS, macOS's `posix_spawnp` searches `PATH` by attempting a real spawn in each
//! directory, so every directory that lacks the program costs a process (and a PID) that the
//! kernel creates and tears down. This file is its own test binary because it sets the process's
//! `PATH`, which no concurrently running test may observe mid-change.

use std::ffi::OsString;
use std::io::ErrorKind;
use std::process::Command;

use procpilot::{Cmd, RunError};
use tempfile::TempDir;

const PP_ARG0: &str = env!("CARGO_BIN_EXE_pp_arg0");
const PP_ECHO: &str = env!("CARGO_BIN_EXE_pp_echo");

/// Directories on `PATH` ahead of the one holding the program.
const MISSES: usize = 20;

/// A fresh process's PID, for measuring how many PIDs something in between consumed.
fn probe_pid() -> u32 {
    let mut child = Command::new(PP_ECHO).spawn().expect("pp_echo spawns by absolute path");
    let pid = child.id();
    child.wait().expect("pp_echo exits");
    pid
}

/// PIDs consumed by `action`, or `None` when the PID counter wrapped around meanwhile.
fn pids_consumed(action: impl FnOnce()) -> Option<u32> {
    let before = probe_pid();
    action();
    let after = probe_pid();
    after.checked_sub(before).map(|d| d - 1)
}

#[test]
fn a_bare_name_spawns_one_process_and_keeps_its_name() {
    let bin = TempDir::new().unwrap();
    std::fs::copy(PP_ARG0, bin.path().join("pp_arg0")).unwrap();
    let misses: Vec<TempDir> = (0..MISSES).map(|_| TempDir::new().unwrap()).collect();
    let path: OsString = std::env::join_paths(misses.iter().map(TempDir::path).chain([bin.path()])).unwrap();
    // SAFETY: this test binary holds this one test, so no other thread reads the environment.
    unsafe { std::env::set_var("PATH", &path) };

    let out = Cmd::new("pp_arg0").run().unwrap();
    assert_eq!(out.stdout_lossy().trim(), "pp_arg0", "the program still sees the name it was given as argv[0]");

    // Other processes on the machine take PIDs too, so take the quietest of several runs. Left to
    // posix_spawnp on macOS, every run costs MISSES + 1.
    let fewest = (0..5)
        .filter_map(|_| {
            pids_consumed(|| {
                Cmd::new("pp_arg0").run().unwrap();
            })
        })
        .min()
        .unwrap();
    assert!(fewest < 10, "spawning by bare name consumed {fewest} PIDs; one process should take one");

    let err = Cmd::new("pp_no_such_program").run().unwrap_err();
    assert!(
        matches!(&err, RunError::Spawn { source, .. } if source.kind() == ErrorKind::NotFound),
        "a name found nowhere fails as it always did: {err:?}"
    );

    #[cfg(feature = "tokio")]
    {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let out = rt.block_on(Cmd::new("pp_arg0").run_async()).unwrap();
        assert_eq!(out.stdout_lossy().trim(), "pp_arg0", "the async path keeps argv[0] too");
        let fewest = (0..5)
            .filter_map(|_| {
                pids_consumed(|| {
                    rt.block_on(Cmd::new("pp_arg0").run_async()).unwrap();
                })
            })
            .min()
            .unwrap();
        assert!(fewest < 10, "the async path consumed {fewest} PIDs for one process");
    }

    let only_bin = Cmd::new("pp_arg0").env("PATH", bin.path()).run().unwrap();
    assert_eq!(only_bin.stdout_lossy().trim(), "pp_arg0", "a per-command PATH is still searched");
    let empty_path = Cmd::new("pp_arg0").env("PATH", misses[0].path()).run().unwrap_err();
    assert!(
        matches!(&empty_path, RunError::Spawn { source, .. } if source.kind() == ErrorKind::NotFound),
        "a per-command PATH without the program is not overridden by the process's own: {empty_path:?}"
    );
}
