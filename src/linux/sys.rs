// SPDX-FileCopyrightText: 2026 Eric Tian <thxdaemon@gmail.com>
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Thin wrappers around the syscalls the sandbox needs.

use anyhow::{Context, Result, bail};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

fn cvt(ret: i64, what: &str) -> Result<i64> {
    if ret < 0 {
        Err(std::io::Error::last_os_error()).with_context(|| what.to_string())
    } else {
        Ok(ret)
    }
}

pub(crate) fn check(ret: i32, what: &str) -> Result<()> {
    cvt(ret as i64, what).map(|_| ())
}

fn write_file(path: &str, data: &str) -> Result<()> {
    std::fs::write(path, data).with_context(|| format!("writing {}", path))
}

/// read(2), retrying a read that a signal interrupted. Zero means the peer
/// closed the pipe.
pub(crate) fn read_some(fd: RawFd, buf: &mut [u8]) -> std::io::Result<usize> {
    loop {
        let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };

        if n >= 0 {
            return Ok(n as usize);
        }

        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

/// Fill the buffer completely, retrying interrupted and short reads. A peer
/// that closes early is an error: the caller asked for all of it.
pub(crate) fn read_full(fd: RawFd, buf: &mut [u8]) -> std::io::Result<()> {
    let mut filled = 0;

    while filled < buf.len() {
        let n = read_some(fd, &mut buf[filled..])?;

        if n == 0 {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }

        filled += n;
    }

    Ok(())
}

/// Write the whole buffer, retrying interrupted and short writes.
pub(crate) fn write_all(fd: RawFd, buf: &[u8]) -> std::io::Result<()> {
    let mut written = 0;

    while written < buf.len() {
        let n = unsafe { libc::write(fd, buf[written..].as_ptr().cast(), buf.len() - written) };

        if n > 0 {
            written += n as usize;
            continue;
        }

        if n == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }

        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::Interrupted {
            return Err(error);
        }
    }

    Ok(())
}

/// Open the calling process's namespace of the given kind, so that another
/// process can join it later.
pub(crate) fn open_namespace(kind: &str) -> Result<OwnedFd> {
    let path = format!("/proc/self/ns/{kind}");
    let file = std::fs::File::open(&path).with_context(|| format!("opening {path}"))?;
    Ok(file.into())
}

/// Write the uid/gid maps of the current (freshly unshared) user namespace.
///
/// Both maps are single entries: `inner_uid` is the id the process will have
/// inside the namespace and `outer_uid` the id it maps to in the *parent*
/// namespace. Single-entry maps are all an unprivileged writer may create.
/// The id of the caller inside the namespace becomes `inner_uid`.
pub(crate) fn setup_id_map(
    inner_uid: u32,
    outer_uid: u32,
    inner_gid: u32,
    outer_gid: u32,
) -> Result<()> {
    // Required before an unprivileged gid_map write. Fails harmlessly when
    // setgroups is already denied in the parent namespace (inherited state).
    let _ = std::fs::write("/proc/self/setgroups", "deny");

    write_file(
        "/proc/self/uid_map",
        &format!("{inner_uid} {outer_uid} 1\n"),
    )?;
    write_file(
        "/proc/self/gid_map",
        &format!("{inner_gid} {outer_gid} 1\n"),
    )?;

    Ok(())
}

#[repr(C)]
struct CapHeader {
    version: u32,
    pid: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

const CAP_VERSION_3: u32 = 0x2008_0522;

unsafe extern "C" {
    fn capset(hdr: *mut CapHeader, data: *const CapData) -> libc::c_int;
}

/// Drop every capability, including from the bounding set, so that a
/// fakeroot (uid 0) shell cannot regain them through execve.
///
/// The call must fail loudly: a caller that continues with capabilities it
/// meant to drop holds powers it was supposed to give up.
pub(crate) fn drop_all_caps() -> Result<()> {
    unsafe {
        let mut hdr = CapHeader {
            version: CAP_VERSION_3,
            pid: 0,
        };

        let zero = CapData {
            effective: 0,
            permitted: 0,
            inheritable: 0,
        };

        // The bounding set must go first: PR_CAPBSET_DROP needs CAP_SETPCAP,
        // which capset() is about to remove. The kernel refuses numbers above
        // its CAP_LAST_CAP with EINVAL, so the loop asks the kernel where the
        // range ends; without that file, an EINVAL can only mean a capability
        // the kernel does not know, and the rest would fail the same way.
        let last = cap_last_cap();
        for cap in 0..=last.unwrap_or(u32::MAX) {
            if libc::prctl(libc::PR_CAPBSET_DROP, cap as libc::c_ulong, 0, 0, 0) != 0 {
                let err = std::io::Error::last_os_error();
                if last.is_none() && err.raw_os_error() == Some(libc::EINVAL) {
                    break;
                }
                return Err(err).with_context(|| format!("prctl(PR_CAPBSET_DROP, {cap})"));
            }
        }

        let data = [zero, zero];
        if capset(&mut hdr, data.as_ptr()) != 0 {
            return Err(std::io::Error::last_os_error()).context("capset");
        }
    }

    Ok(())
}

/// The highest capability the running kernel knows, from the file the kernel
/// provides for exactly this purpose (since Linux 2.6.34).
fn cap_last_cap() -> Option<u32> {
    std::fs::read_to_string("/proc/sys/kernel/cap_last_cap")
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// Get or clear the dumpable flag. A non-dumpable process cannot be traced
/// or inspected through /proc by same-uid processes without CAP_SYS_PTRACE.
/// The flag survives until credentials change or a normal binary is exec'd.
pub(crate) fn set_dumpable(dumpable: bool) -> Result<()> {
    let flag = if dumpable { 1 } else { 0 };
    check(
        unsafe { libc::prctl(libc::PR_SET_DUMPABLE, flag, 0, 0, 0) },
        "prctl(PR_SET_DUMPABLE)",
    )
}

pub(crate) fn set_no_new_privs() -> Result<()> {
    check(
        unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) },
        "prctl(PR_SET_NO_NEW_PRIVS)",
    )
}

/// The name ps(1), top(1) and /proc/<pid>/comm show for this process.
///
/// The kernel takes at most 15 bytes, so a long sandbox name is truncated
/// rather than rejected. `comm_bytes` is separate so the truncation can be
/// tested without renaming the test process.
fn comm_bytes(name: &str) -> [u8; 16] {
    let mut buf = [0u8; 16];
    let mut end = 0;

    for ch in name.chars() {
        let mut encoded = [0u8; 4];

        let bytes = ch.encode_utf8(&mut encoded).as_bytes();
        if end + bytes.len() > 15 {
            break;
        }

        buf[end..end + bytes.len()].copy_from_slice(bytes);
        end += bytes.len();
    }

    buf
}

pub(crate) fn set_process_name(name: &str) -> Result<()> {
    let buf = comm_bytes(name);
    check(
        unsafe { libc::prctl(libc::PR_SET_NAME, buf.as_ptr(), 0, 0, 0) },
        "prctl(PR_SET_NAME)",
    )
}

// ---------------------------------------------------------------------------
// disabling further user namespaces
// ---------------------------------------------------------------------------

/// Lower the user namespace budget of the current user namespace.
///
/// The limit is accounted against this namespace and every namespace below
/// it, so spending it on one more namespace leaves the sandbox with none.
pub(crate) fn limit_user_namespaces(max: u32) -> Result<()> {
    let path = "/proc/sys/user/max_user_namespaces";
    write_file(path, &max.to_string()).with_context(|| format!("writing {}", path))
}

/// Creating a user namespace has to fail at this point; if it does not, the
/// sandbox is not what we asked for and we must not run in it.
pub(crate) fn assert_userns_disabled() -> Result<()> {
    if unsafe { libc::unshare(libc::CLONE_NEWUSER) } == 0 {
        bail!("user namespace creation was not disabled as requested");
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// signal notification
// ---------------------------------------------------------------------------

/// A signalfd: the listed signals are blocked and delivered as records on a
/// file descriptor instead, so a poll loop can react to them without a signal
/// handler and without a self-pipe.
pub(crate) struct SignalFd {
    fd: OwnedFd,
}

impl SignalFd {
    pub fn new(signals: &[libc::c_int]) -> Result<SignalFd> {
        unsafe {
            let mut set: libc::sigset_t = std::mem::zeroed();

            if libc::sigemptyset(&mut set) != 0 {
                return Err(std::io::Error::last_os_error()).context("sigemptyset");
            }

            for sig in signals {
                if libc::sigaddset(&mut set, *sig) != 0 {
                    return Err(std::io::Error::last_os_error())
                        .with_context(|| format!("sigaddset({sig})"));
                }
            }

            // The signals have to be blocked before the descriptor exists:
            // otherwise they are delivered normally instead of being queued.
            if libc::sigprocmask(libc::SIG_BLOCK, &set, std::ptr::null_mut()) != 0 {
                return Err(std::io::Error::last_os_error()).context("sigprocmask");
            }

            let fd = libc::signalfd(-1, &set, libc::SFD_NONBLOCK | libc::SFD_CLOEXEC);
            if fd < 0 {
                return Err(std::io::Error::last_os_error()).context("signalfd");
            }

            Ok(SignalFd {
                fd: OwnedFd::from_raw_fd(fd),
            })
        }
    }

    /// Drain the pending signals. Never blocks.
    pub fn read(&self) -> Vec<libc::c_int> {
        let mut signals = Vec::new();

        let size = std::mem::size_of::<libc::signalfd_siginfo>();

        loop {
            let mut info: libc::signalfd_siginfo = unsafe { std::mem::zeroed() };

            // SAFETY: the slice covers exactly one signalfd_siginfo.
            let bytes = unsafe {
                std::slice::from_raw_parts_mut(
                    &mut info as *mut libc::signalfd_siginfo as *mut u8,
                    size,
                )
            };

            match read_some(self.fd.as_raw_fd(), bytes) {
                Ok(n) if n == size => signals.push(info.ssi_signo as libc::c_int),
                _ => break,
            }
        }

        signals
    }
}

impl AsRawFd for SignalFd {
    fn as_raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

// ---------------------------------------------------------------------------
// misc
// ---------------------------------------------------------------------------

pub(crate) fn is_tty(fd: RawFd) -> bool {
    unsafe { libc::isatty(fd) == 1 }
}

#[cfg(test)]
mod tests {
    use super::{
        assert_userns_disabled, comm_bytes, drop_all_caps, limit_user_namespaces, read_full,
        setup_id_map, write_all,
    };
    use crate::linux::testing::{
        enter_first_level, host_gid, host_uid, run_in_child, uid_map_line,
        user_namespaces_available,
    };
    use nix::sched::{CloneFlags, unshare};
    use nix::unistd::{Gid, Uid};
    use std::os::fd::AsRawFd;

    #[test]
    fn a_pipe_round_trip_moves_every_byte() {
        let (read, write) = nix::unistd::pipe().expect("pipe");
        let sent = [1u8, 2, 3, 4, 5];

        write_all(write.as_raw_fd(), &sent).expect("write");

        let mut got = [0u8; 5];
        read_full(read.as_raw_fd(), &mut got).expect("read");
        assert_eq!(got, sent);
    }

    #[test]
    fn a_closed_pipe_is_not_a_short_message() {
        let (read, write) = nix::unistd::pipe().expect("pipe");
        drop(write);

        let mut got = [0u8; 5];
        let error = read_full(read.as_raw_fd(), &mut got).expect_err("closed pipe");
        assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
    }

    fn cap_eff() -> Option<String> {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|status| {
                status
                    .lines()
                    .find_map(|line| line.strip_prefix("CapEff:"))
                    .map(|value| value.trim().to_string())
            })
    }

    fn shown(name: &str) -> String {
        let buf = comm_bytes(name);
        let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
        String::from_utf8_lossy(&buf[..end]).into_owned()
    }

    #[test]
    fn root_mapped_first_level_can_write_userns_budget() {
        // Pins the sequence the initializer relies on: the budget sysctl is
        // written in-process, right after the first level is entered, so the
        // capabilities granted at unshare() time are still held. The root
        // mapping is not what makes the write work (an identity map works
        // too while those capabilities are held); it exists so that every
        // mount happens in a namespace where uid 0 is mapped.
        if !user_namespaces_available() {
            eprintln!("skip: user namespaces unavailable");
            return;
        }
        let (uid, gid) = (host_uid(), host_gid());
        let code = run_in_child(move || {
            if !enter_first_level(uid, gid) {
                return 9;
            }
            if let Err(e) = limit_user_namespaces(1) {
                eprintln!("limit_user_namespaces: {:#}", e);
                return 1;
            }
            match std::fs::read_to_string("/proc/sys/user/max_user_namespaces") {
                Ok(v) if v.trim() == "1" => 0,
                other => {
                    eprintln!("unexpected sysctl value {other:?}");
                    2
                }
            }
        });
        assert_eq!(code, Some(0));
    }

    #[test]
    fn identity_map_keeps_caller_uid() {
        if !user_namespaces_available() {
            eprintln!("skip: user namespaces unavailable");
            return;
        }
        let (uid, gid) = (host_uid(), host_gid());
        let code = run_in_child(move || {
            if unshare(CloneFlags::CLONE_NEWUSER).is_err() {
                return 9;
            }
            if setup_id_map(uid, uid, gid, gid).is_err() {
                return 8;
            }
            let expected = format!("{uid} {uid} 1");
            if Uid::current().as_raw() != uid
                || Gid::current().as_raw() != gid
                || uid_map_line().as_deref() != Some(expected.as_str())
            {
                return 1;
            }
            0
        });
        assert_eq!(code, Some(0));
    }

    /// The host may refuse a second-level user namespace (a container that is
    /// already nested, an LSM). The tests below then have nothing to measure.
    fn second_level_supported() -> bool {
        run_in_child(|| {
            let (uid, gid) = (host_uid(), host_gid());
            if !enter_first_level(uid, gid) {
                return 1;
            }
            let probe = run_in_child(|| {
                if unshare(CloneFlags::CLONE_NEWUSER).is_err() {
                    return 1;
                }
                0
            });
            match probe {
                Some(0) => 0,
                _ => 2,
            }
        }) == Some(0)
    }

    #[test]
    fn a_second_level_user_namespace_remaps_the_visible_ids() {
        // The sandbox runs this sequence in setup::enter, on top of a first
        // level that mapped root onto the caller.
        if !user_namespaces_available() {
            eprintln!("skip: user namespaces unavailable");
            return;
        }
        if !second_level_supported() {
            eprintln!("skip: host forbids a second-level user namespace");
            return;
        }
        let (uid, gid) = (host_uid(), host_gid());

        let code = run_in_child(move || {
            if !enter_first_level(uid, gid) {
                return 9;
            }
            if limit_user_namespaces(1).is_err() {
                return 1;
            }
            if unshare(CloneFlags::CLONE_NEWUSER).is_err() {
                return 2;
            }
            if setup_id_map(uid, 0, gid, 0).is_err() {
                return 3;
            }
            if assert_userns_disabled().is_err() {
                return 4;
            }
            let expected = format!("{uid} 0 1");
            if Uid::current().as_raw() != uid
                || Gid::current().as_raw() != gid
                || uid_map_line().as_deref() != Some(expected.as_str())
            {
                return 5;
            }
            0
        });
        assert_eq!(code, Some(0), "non-fakeroot second level");

        let code = run_in_child(move || {
            if !enter_first_level(uid, gid) {
                return 9;
            }
            if limit_user_namespaces(1).is_err() {
                return 1;
            }
            if unshare(CloneFlags::CLONE_NEWUSER).is_err() {
                return 2;
            }
            if setup_id_map(0, 0, 0, 0).is_err() {
                return 3;
            }
            if assert_userns_disabled().is_err() {
                return 4;
            }
            if Uid::current().as_raw() != 0
                || Gid::current().as_raw() != 0
                || uid_map_line().as_deref() != Some("0 0 1")
            {
                return 5;
            }
            0
        });
        assert_eq!(code, Some(0), "fakeroot second level");
    }

    #[test]
    fn a_nested_second_level_leaves_room_for_another_namespace() {
        if !user_namespaces_available() {
            eprintln!("skip: user namespaces unavailable");
            return;
        }
        if !second_level_supported() {
            eprintln!("skip: host forbids a second-level user namespace");
            return;
        }
        let (uid, gid) = (host_uid(), host_gid());
        let code = run_in_child(move || {
            if !enter_first_level(uid, gid) {
                return 9;
            }
            // Nested mode neither cuts the budget nor asserts it is spent.
            if unshare(CloneFlags::CLONE_NEWUSER).is_err() {
                return 1;
            }
            if setup_id_map(uid, 0, gid, 0).is_err() {
                return 2;
            }
            if Uid::current().as_raw() != uid {
                return 3;
            }
            // Creating a further user namespace has to succeed. ENOSPC here
            // would mean the budget was spent after all; EPERM is the
            // environment refusing deeper nesting, not something sbox did.
            match unshare(CloneFlags::CLONE_NEWUSER) {
                Ok(()) => 0,
                Err(nix::errno::Errno::ENOSPC) => {
                    eprintln!("nested mode cut the userns budget");
                    4
                }
                Err(e) => {
                    eprintln!("skip: environment refuses deeper nesting ({e})");
                    77
                }
            }
        });
        match code {
            Some(0) => {}
            Some(77) => eprintln!("skip: environment refuses deeper nesting"),
            other => panic!("nested mode broke userns creation: {other:?}"),
        }
    }

    #[test]
    fn dropping_caps_clears_the_effective_set() {
        // A fresh user namespace grants every capability, so dropping them
        // has something to drop and the result is observable: the effective
        // set has to come out empty.
        if !user_namespaces_available() {
            eprintln!("skip: user namespaces unavailable");
            return;
        }
        let (uid, gid) = (host_uid(), host_gid());
        let code = run_in_child(move || {
            if !enter_first_level(uid, gid) {
                return 9;
            }
            if let Err(e) = drop_all_caps() {
                eprintln!("drop_all_caps: {e:#}");
                return 1;
            }
            match cap_eff() {
                Some(set) if set.chars().all(|c| c == '0') => 0,
                other => {
                    eprintln!("CapEff after dropping everything: {other:?}");
                    2
                }
            }
        });
        assert_eq!(code, Some(0));
    }

    #[test]
    fn process_names_fit_in_fifteen_bytes() {
        assert_eq!(shown("sbox:work"), "sbox:work");
        assert_eq!(shown("sbox:abcdefghij"), "sbox:abcdefghij");
        assert_eq!(shown("sbox:abcdefghijk"), "sbox:abcdefghij");
        // Never split a multi-byte character: five two-byte characters fit
        // in the 15 bytes the kernel takes, a sixth does not.
        assert_eq!(
            shown("sbox:\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}"),
            "sbox:\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}"
        );
    }
}
