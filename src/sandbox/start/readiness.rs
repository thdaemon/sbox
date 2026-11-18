// SPDX-FileCopyrightText: 2026 Eric Tian <thxdaemon@gmail.com>
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The readiness pipe back to whoever asked for a sandbox.
//!
//! The setup process holds the write end and reports exactly once: 'R' the
//! sandbox is up, 'B' another process owns the name, 'E' setup failed. The
//! caller holds the read end and waits for that report or for a timeout.
//!
//! Neither side deals in bytes. The writer has one method per outcome and the
//! reader returns a Report, so the protocol is stated once, here.

use crate::linux::sys;
use anyhow::{Context, Result};
use nix::fcntl::OFlag;
use std::os::fd::{AsRawFd, OwnedFd};
use std::time::{Duration, Instant};

/// Both ends of the readiness pipe: the writer goes to the setup process, the
/// watch stays with whoever waits for the sandbox.
pub(super) fn channel() -> Result<(Readiness, ReadyWatch)> {
    let (watch, readiness) = nix::unistd::pipe2(OFlag::O_CLOEXEC).context("pipe2")?;
    Ok((Readiness(readiness), ReadyWatch(watch)))
}

/// What the setup process reported about the start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Report {
    /// The sandbox is up: the control socket accepts and the namespace
    /// descriptors can be handed out.
    Ready,
    /// Another process owns the name; attach to that sandbox instead of
    /// starting a second one.
    Busy,
    /// Setup failed, or died before it could say anything. When there is a
    /// reason, the setup process prints it on the caller's terminal before it
    /// goes away.
    Failed,
    /// Nothing arrived in time: the setup process may be stuck.
    Timeout,
}

/// The write end: held by the setup process, used once.
pub(super) struct Readiness(OwnedFd);

impl Readiness {
    /// The sandbox is up. Dropping the pipe end tells the caller there is
    /// nothing more to come.
    pub fn ready(self) {
        // Best effort: a report that cannot be delivered has nobody left to
        // tell.
        let _ = sys::write_all(self.0.as_raw_fd(), b"R");
    }

    /// Another process owns the name; the caller attaches to that sandbox
    /// instead of starting a second one.
    pub fn busy(self) {
        let _ = sys::write_all(self.0.as_raw_fd(), b"B");
    }

    /// Setup failed. The setup process prints the reason before it exits.
    pub fn failed(self) {
        let _ = sys::write_all(self.0.as_raw_fd(), b"E");
    }
}

/// The read end: held by the caller while it waits for the sandbox to come up.
pub(super) struct ReadyWatch(OwnedFd);

impl ReadyWatch {
    /// Block until the setup process reports, or until the timeout runs out.
    ///
    /// A closed pipe is not a timeout: it means the writer is gone, so waiting
    /// longer cannot help.
    pub fn wait(&self, timeout: Duration) -> Result<Report> {
        let start = Instant::now();

        loop {
            let left = timeout.saturating_sub(start.elapsed());
            if left.is_zero() {
                return Ok(Report::Timeout);
            }

            let mut pfd = [libc::pollfd {
                fd: self.0.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            }];

            let rc = unsafe { libc::poll(pfd.as_mut_ptr(), 1, left.as_millis() as i32) };
            if rc < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err).context("poll");
            }
            if rc == 0 {
                return Ok(Report::Timeout);
            }

            let mut byte = 0u8;
            let n = sys::read_some(self.0.as_raw_fd(), std::slice::from_mut(&mut byte))
                .context("read")?;

            return Ok(match n {
                0 => Report::Failed,
                _ => match byte {
                    b'R' => Report::Ready,
                    b'B' => Report::Busy,
                    _ => Report::Failed,
                },
            });
        }
    }
}

/// Why a sandbox was not started. Busy is not a failure: the name is taken and
/// the caller should attach to the sandbox that owns it.
pub(super) enum Fault {
    Busy,
    Failed(anyhow::Error),
}

impl From<anyhow::Error> for Fault {
    fn from(err: anyhow::Error) -> Fault {
        Fault::Failed(err)
    }
}

impl From<nix::errno::Errno> for Fault {
    fn from(err: nix::errno::Errno) -> Fault {
        Fault::Failed(err.into())
    }
}

#[cfg(test)]
mod tests {
    use super::{Report, channel};
    use crate::linux::sys;
    use std::os::fd::AsRawFd;
    use std::time::{Duration, Instant};

    #[test]
    fn ready_is_reported() {
        let (readiness, watch) = channel().expect("channel");
        let writer = std::thread::spawn(move || readiness.ready());
        assert_eq!(
            watch.wait(Duration::from_secs(5)).expect("wait"),
            Report::Ready
        );
        writer.join().expect("writer");
    }

    #[test]
    fn a_silent_writer_times_out() {
        let (readiness, watch) = channel().expect("channel");
        assert_eq!(
            watch.wait(Duration::from_millis(20)).expect("wait"),
            Report::Timeout
        );
        drop(readiness);
    }

    #[test]
    fn a_closed_pipe_is_a_failure() {
        let (readiness, watch) = channel().expect("channel");
        drop(readiness);
        assert_eq!(
            watch.wait(Duration::from_secs(5)).expect("wait"),
            Report::Failed
        );
    }

    #[test]
    fn the_setup_side_reports_busy_and_failure() {
        for (byte, expected) in [(b'B', Report::Busy), (b'E', Report::Failed)] {
            let (readiness, watch) = channel().expect("channel");
            let start = Instant::now();
            sys::write_all(readiness.0.as_raw_fd(), &[byte]).expect("write");
            drop(readiness);
            assert!(
                start.elapsed() < Duration::from_secs(1),
                "the report must arrive without waiting"
            );
            assert_eq!(watch.wait(Duration::from_secs(5)).expect("wait"), expected);
        }
    }

    #[test]
    fn an_unknown_byte_is_a_failure() {
        let (readiness, watch) = channel().expect("channel");
        sys::write_all(readiness.0.as_raw_fd(), b"X").expect("write");
        drop(readiness);
        assert_eq!(
            watch.wait(Duration::from_secs(5)).expect("wait"),
            Report::Failed
        );
    }
}
