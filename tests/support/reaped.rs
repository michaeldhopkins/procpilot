//! Keeps a child a test spawns from outliving the test.
//!
//! A test that panics, or a mutant that makes a child hang, must not leave the child running:
//! cargo-mutants' temp copies once left three `pp_cat`s orphaned for 90 minutes. Two layers here:
//! [`Reaped`] kills and reaps a spawned handle when the test unwinds, and every wait on a child
//! is bounded by [`DEADLINE`]. If the test process itself is killed, the mocks exit on their own
//! once orphaned (`src/bin/support/orphan.rs`).

use std::ops::{Deref, DerefMut};
use std::time::Duration;

use procpilot::{RunError, RunOutput, SpawnedProcess};

/// Longest any test waits on a child that should already be finishing.
pub const DEADLINE: Duration = Duration::from_secs(10);

/// A [`SpawnedProcess`] whose every stage is killed and reaped when the guard drops, so a
/// failing assertion cannot strand it.
pub struct Reaped(pub SpawnedProcess);

impl Deref for Reaped {
    type Target = SpawnedProcess;
    fn deref(&self) -> &SpawnedProcess {
        &self.0
    }
}

impl DerefMut for Reaped {
    fn deref_mut(&mut self) -> &mut SpawnedProcess {
        &mut self.0
    }
}

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait_timeout(DEADLINE);
    }
}

impl Reaped {
    /// [`SpawnedProcess::wait`], failing the test instead of hanging past [`DEADLINE`].
    pub fn wait_bounded(&self) -> Result<RunOutput, RunError> {
        self.0
            .wait_timeout(DEADLINE)
            .map(|out| out.expect("child still running at the deadline"))
    }
}
