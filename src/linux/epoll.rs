//! The epoll instance, and the trait that says what a registered descriptor
//! carries along with it.
//!
//! Nothing here decides what a token means: the caller packs whatever it needs
//! into the u64, and reads it back from the event.

use anyhow::{Context, Result};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

/// Something epoll can watch: a descriptor, and the token epoll hands back
/// with its events.
///
/// The descriptor and its token travel together, so a registration cannot be
/// made with one and looked up by the other.
pub(crate) trait Source {
    fn fd(&self) -> RawFd;
    fn token(&self) -> u64;
}

/// An epoll instance. Events stay level-triggered, which is what a descriptor
/// per session wants.
pub(crate) struct Epoll {
    fd: OwnedFd,
}

impl Epoll {
    pub fn new() -> Result<Epoll> {
        let fd = unsafe { libc::epoll_create1(libc::EPOLL_CLOEXEC) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error()).context("epoll_create1");
        }

        Ok(Epoll {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
        })
    }

    pub fn add(&self, source: &impl Source, events: i32) -> Result<()> {
        self.control(libc::EPOLL_CTL_ADD, source, events)
    }

    pub fn modify(&self, source: &impl Source, events: i32) -> Result<()> {
        self.control(libc::EPOLL_CTL_MOD, source, events)
    }

    fn control(&self, op: libc::c_int, source: &impl Source, events: i32) -> Result<()> {
        let mut event = libc::epoll_event {
            events: events as u32,
            u64: source.token(),
        };

        let rc = unsafe { libc::epoll_ctl(self.fd.as_raw_fd(), op, source.fd(), &mut event) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error()).context("epoll_ctl");
        }

        Ok(())
    }

    pub fn wait(
        &self,
        events: &mut [libc::epoll_event],
        timeout_ms: i32,
    ) -> std::io::Result<usize> {
        let n = unsafe {
            libc::epoll_wait(
                self.fd.as_raw_fd(),
                events.as_mut_ptr(),
                events.len() as libc::c_int,
                timeout_ms,
            )
        };

        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }

        Ok(n as usize)
    }
}
