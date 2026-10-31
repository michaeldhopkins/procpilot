//! The check made before each attempt, shared by the sync and async runners:
//! stop if cancellation was requested, or if the deadline has already passed.

use std::time::{Duration, Instant};

use super::CancelControl;
use crate::cmd_display::CmdDisplay;
use crate::error::RunError;

/// `Err` when the attempt numbered `attempt` must not start.
///
/// Cancellation is checked first, so it wins over an expired deadline.
pub(super) fn check(
    display: &CmdDisplay,
    cancel: Option<&CancelControl>,
    deadline: Option<Instant>,
    now: Instant,
    attempt: u32,
) -> Result<(), RunError> {
    if cancel.is_some_and(CancelControl::is_set) {
        return Err(RunError::Cancelled {
            command: display.clone(),
            stdout: Vec::new(),
            stderr: String::new(),
            attempts: attempt,
        });
    }
    if deadline.is_some_and(|d| now >= d) {
        return Err(RunError::Timeout {
            command: display.clone(),
            elapsed: Duration::ZERO,
            stdout: Vec::new(),
            stderr: String::new(),
            attempts: attempt,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;

    use super::super::DEFAULT_CANCEL_GRACE;
    use super::*;

    fn display() -> CmdDisplay {
        CmdDisplay::new("prog".into(), Vec::new(), false)
    }

    fn control(set: bool) -> CancelControl {
        CancelControl {
            flag: Arc::new(AtomicBool::new(set)),
            grace: DEFAULT_CANCEL_GRACE,
        }
    }

    #[test]
    fn passes_with_no_cancel_and_no_deadline() {
        assert!(check(&display(), None, None, Instant::now(), 1).is_ok());
    }

    #[test]
    fn passes_with_an_unset_flag_and_a_future_deadline() {
        let now = Instant::now();
        let cancel = control(false);
        let deadline = now + Duration::from_secs(60);
        assert!(check(&display(), Some(&cancel), Some(deadline), now, 1).is_ok());
    }

    #[test]
    fn a_set_flag_cancels_with_the_attempt_number() {
        let cancel = control(true);
        let err = check(&display(), Some(&cancel), None, Instant::now(), 3).unwrap_err();
        assert!(matches!(err, RunError::Cancelled { attempts: 3, .. }));
    }

    #[test]
    fn a_deadline_at_or_before_now_times_out_with_the_attempt_number() {
        let now = Instant::now();
        let err = check(&display(), None, Some(now), now, 2).unwrap_err();
        assert!(matches!(err, RunError::Timeout { attempts: 2, elapsed, .. } if elapsed.is_zero()));
    }

    #[test]
    fn cancellation_wins_over_an_expired_deadline() {
        let now = Instant::now();
        let cancel = control(true);
        let err = check(&display(), Some(&cancel), Some(now), now, 1).unwrap_err();
        assert!(matches!(err, RunError::Cancelled { .. }));
    }
}
