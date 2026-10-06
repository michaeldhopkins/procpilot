//! `Read` for [`SpawnedProcess`], straight from the rightmost stage's stdout.

use std::io::{self, Read};

use super::{SpawnedProcess, StdoutState};

/// Read directly from the rightmost stage's stdout.
///
/// On first read, takes ownership of stdout internally (so subsequent
/// [`take_stdout`](SpawnedProcess::take_stdout) calls return `None`).
/// Reads return `Ok(0)` when stdout closes (EOF). Call
/// [`wait`](SpawnedProcess::wait) after EOF to surface the exit status.
impl Read for SpawnedProcess {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        read_via_handle(self, buf)
    }
}

/// Dual impl enabling `(&proc).read(…)`. Lets one thread read while another
/// holds the handle by reference and calls [`kill`](SpawnedProcess::kill)
/// or [`wait`](SpawnedProcess::wait).
impl Read for &SpawnedProcess {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        read_via_handle(self, buf)
    }
}

fn read_via_handle(p: &SpawnedProcess, buf: &mut [u8]) -> io::Result<usize> {
    let mut guard = p
        .stdout
        .lock()
        .map_err(|_| io::Error::other("stdout mutex poisoned"))?;
    if matches!(*guard, StdoutState::NotTaken) {
        match p.children.last().and_then(|c| c.take_stdout()) {
            Some(pipe) => *guard = StdoutState::Cached(pipe),
            None => *guard = StdoutState::GivenAway,
        }
    }
    match &mut *guard {
        StdoutState::Cached(pipe) => pipe.read(buf),
        StdoutState::NotTaken | StdoutState::GivenAway => Ok(0),
    }
}
