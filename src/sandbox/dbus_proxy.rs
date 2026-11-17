//! Filtered dbus proxies, started outside the sandbox and bound into it.
//!
//! The caller resolves the policy: upstream address, socket path and the final
//! rule list for each bus. This module only owns the process lifecycle.

use crate::linux::sys;
use anyhow::{Context, Result, bail};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::PathBuf;
use std::process::{Command, Stdio};

/// One bus the proxy should expose.
pub(super) struct BusSpec {
    address: String,
    socket: PathBuf,
    rules: Vec<String>,
}

impl BusSpec {
    pub fn new(address: String, socket: PathBuf, rules: Vec<String>) -> BusSpec {
        BusSpec {
            address,
            socket,
            rules,
        }
    }
}

/// Configure and start the filtered dbus proxies.
///
/// The value is consumed by start(); what remains is the sockets the mount
/// tree binds and the guard that keeps the proxy alive.
#[derive(Default)]
pub(super) struct DbusProxy {
    debug: bool,
    session: Option<BusSpec>,
    system: Option<BusSpec>,
    /// Where the proxy writes its diagnostics; None keeps it quiet.
    log: Option<PathBuf>,
}

impl DbusProxy {
    pub fn new() -> DbusProxy {
        DbusProxy::default()
    }

    pub fn debug(mut self, debug: bool) -> Self {
        self.debug = debug;
        self
    }

    pub fn session(mut self, session: Option<BusSpec>) -> Self {
        self.session = session;
        self
    }

    pub fn system(mut self, system: Option<BusSpec>) -> Self {
        self.system = system;
        self
    }

    /// Write the proxy's diagnostics to `log`; None silences it.
    pub fn log(mut self, log: Option<PathBuf>) -> Self {
        self.log = log;
        self
    }

    /// Start xdg-dbus-proxy for the configured buses.
    ///
    /// Returns the sockets to bind into the sandbox and the readiness guard.
    /// Dropping the guard stops the proxy.
    pub fn start(self) -> Result<(DbusProxySockets, Option<OwnedFd>)> {
        let DbusProxy {
            debug,
            session,
            system,
            log,
        } = self;

        let session = session.as_ref();
        let system = system.as_ref();

        if session.is_none() && system.is_none() {
            return Ok((DbusProxySockets::default(), None));
        }

        for bus in [session, system].into_iter().flatten() {
            let _ = std::fs::remove_file(&bus.socket);
        }

        // Readiness pipe: xdg-dbus-proxy writes one byte once every bus is up.
        let mut fds = [0 as RawFd; 2];
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error()).context("pipe");
        }

        let (ready_r, ready_w) = (fds[0], fds[1]);
        unsafe {
            libc::fcntl(ready_r, libc::F_SETFD, libc::FD_CLOEXEC);
        }

        let mut cmd = Command::new("xdg-dbus-proxy");

        // The proxy outlives setup, so it must not keep the caller's terminal.
        // Its log is opened here, before the mount tree hides the host path;
        // without one it stays quiet.
        let output = log.and_then(|path| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .ok()
        });
        cmd.stdin(Stdio::null());
        match output {
            Some(log) => {
                let errors = log.try_clone().context("duplicating the proxy log")?;
                cmd.stdout(Stdio::from(log));
                cmd.stderr(Stdio::from(errors));
            }
            None => {
                cmd.stdout(Stdio::null());
                cmd.stderr(Stdio::null());
            }
        }

        if let Some(bus) = session {
            cmd.arg(&bus.address).arg(&bus.socket).arg("--filter");
            if debug {
                cmd.arg("--log");
            }

            for rule in &bus.rules {
                cmd.arg(format!("--{rule}"));
            }
        }

        if let Some(bus) = system {
            cmd.arg(&bus.address).arg(&bus.socket).arg("--filter");
            if debug {
                cmd.arg("--log");
            }

            for rule in &bus.rules {
                cmd.arg(format!("--{rule}"));
            }
        }

        cmd.arg(format!("--fd={}", ready_w));

        let child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                unsafe {
                    libc::close(ready_r);
                    libc::close(ready_w);
                }
                bail!("cannot start xdg-dbus-proxy: {}", e);
            }
        };

        unsafe {
            libc::close(ready_w);
        }

        // The proxy exits when the read end of the readiness pipe is closed, so
        // it is kept open for as long as the sandbox lives.
        let ready_guard = unsafe { OwnedFd::from_raw_fd(ready_r) };

        // Wait for the proxy to become ready.
        let mut pfd = [libc::pollfd {
            fd: ready_r,
            events: libc::POLLIN,
            revents: 0,
        }];
        let rc = unsafe { libc::poll(pfd.as_mut_ptr(), 1, 5000) };
        let ok = rc == 1 && pfd[0].revents & libc::POLLIN != 0;
        if !ok {
            bail!("xdg-dbus-proxy did not become ready");
        }

        // Consume the readiness byte so the pipe is only a lifetime guard from
        // here on.
        let mut ready = [0u8; 1];
        match sys::read_full(ready_guard.as_raw_fd(), &mut ready) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                bail!("xdg-dbus-proxy did not become ready");
            }
            Err(e) => return Err(e).context("reading the readiness byte"),
        }

        // The proxy is not a child of the sandbox process that keeps the
        // guard, so drop the handle without killing or reaping it.
        drop(child);

        Ok((
            DbusProxySockets {
                session: session.map(|bus| bus.socket.clone()),
                system: system.map(|bus| bus.socket.clone()),
            },
            Some(ready_guard),
        ))
    }
}

#[derive(Debug, Clone, Default)]
pub(super) struct DbusProxySockets {
    pub session: Option<PathBuf>,
    pub system: Option<PathBuf>,
}
