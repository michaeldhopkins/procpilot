//! Makes a mock exit once its parent is gone.
//!
//! A test process killed mid-test (cargo-mutants' timeout, a Ctrl-C) never runs its drop guards,
//! so a mock blocked on a silent stdin or a long sleep would outlive it, reparented to init.
//! Three `pp_cat`s did, for 90 minutes. Polling the parent pid works on every Unix, where Linux's
//! `PR_SET_PDEATHSIG` has no macOS equivalent.

#[cfg(unix)]
pub fn exit_when_orphaned() {
    use std::os::unix::process::parent_id;
    use std::time::Duration;

    let parent = parent_id();
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_millis(100));
            if parent_id() != parent {
                std::process::exit(125);
            }
        }
    });
}

#[cfg(not(unix))]
pub fn exit_when_orphaned() {}
