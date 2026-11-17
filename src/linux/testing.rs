//! Helpers for tests that need a forked child, and sometimes a user
//! namespace. Nothing here is compiled outside `cargo test`.
//!
//! The child may only do syscall-level work and has to leave through _exit:
//! cargo runs tests in threads, so unwinding or running destructors there
//! would touch state other threads own.

use crate::linux::sys;
use nix::sched::{CloneFlags, unshare};
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{ForkResult, Gid, Uid, fork};
use std::io::Read;

pub(crate) fn host_uid() -> u32 {
    Uid::current().as_raw()
}

pub(crate) fn host_gid() -> u32 {
    Gid::current().as_raw()
}

/// Run `f` in a forked child and return its exit code.
pub(crate) fn run_in_child(f: impl FnOnce() -> i32) -> Option<i32> {
    match unsafe { fork() } {
        Ok(ForkResult::Child) => {
            let code = f();
            unsafe { libc::_exit(code) }
        }
        Ok(ForkResult::Parent { child }) => loop {
            match waitpid(Some(child), Some(WaitPidFlag::empty())) {
                Ok(WaitStatus::Exited(_, code)) => return Some(code),
                Ok(WaitStatus::Signaled(_, sig, _)) => return Some(128 + sig as i32),
                Ok(_) => continue,
                Err(_) => return None,
            }
        },
        Err(_) => None,
    }
}

/// Enter a user namespace that maps uid/gid 0 onto the caller.
pub(crate) fn enter_first_level(uid: u32, gid: u32) -> bool {
    unshare(CloneFlags::CLONE_NEWUSER).is_ok() && sys::setup_id_map(0, uid, 0, gid).is_ok()
}

/// The first line of /proc/self/uid_map, with the kernel's column padding
/// normalized, so a test can compare it as one string.
pub(crate) fn uid_map_line() -> Option<String> {
    let mut map = String::new();
    std::fs::File::open("/proc/self/uid_map")
        .ok()?
        .read_to_string(&mut map)
        .ok()?;
    Some(
        map.lines()
            .next()?
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
    )
}

pub(crate) fn user_namespaces_available() -> bool {
    run_in_child(|| {
        if unshare(CloneFlags::CLONE_NEWUSER).is_err() {
            return 1;
        }
        0
    }) == Some(0)
}
