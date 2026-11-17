//! Attach: join an existing sandbox and run a command on the caller's tty.
//!
//! The sandbox hands out the descriptors of its namespaces over the control
//! socket: the first-level user namespace (which grants the capabilities the
//! following joins need), mount, uts, ipc, pid and the second-level user
//! namespace. Every one of them is joined with setns(); the order matters,
//! because each step needs the capabilities the previous one provides and a
//! namespace can only be entered on the way down.
//!
//! No pty is allocated: the command inherits the terminal of whoever ran
//! sbox attach, so job control and terminal signals keep working the way they
//! do for any other foreground process.

use crate::linux::sys;
use crate::protocol;
use anyhow::{Context, Result, bail};
use nix::sched::{CloneFlags, setns};
use nix::sys::wait::{WaitStatus, waitpid};
use nix::unistd::{ForkResult, Pid, execvp, fork, getppid};
use std::ffi::CString;
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;

/// A session in a sandbox: its namespaces have been joined and its
/// environment is set, so all that is left is to become the command.
pub(crate) struct Session {
    /// The connection *is* the session: the sandbox counts open connections
    /// and removes itself when the last one closes. Held until the command has
    /// been reaped, never read.
    _lease: UnixStream,
}

impl Session {
    /// Join the sandbox described by the attach reply and keep the lease that
    /// holds it open. The reply and the descriptors are consumed here: once
    /// setns() has run, only the lease has to outlive the command.
    pub fn join(
        lease: UnixStream,
        reply: protocol::AttachReply,
        fds: Vec<OwnedFd>,
    ) -> Result<Session> {
        Self::enter(reply, fds)?;

        Ok(Session { _lease: lease })
    }

    /// Exec the command on the caller's terminal and return its exit status.
    /// The lease drops with the session, after the command is gone.
    pub fn run(self, argv: Vec<String>) -> Result<i32> {
        let child = Self::spawn(argv)?;

        Self::reap(child)
    }

    /// Join the namespaces in `fds` and apply the environment of `reply`.
    ///
    /// The descriptors are only needed for setns() and the reply only to name
    /// them and set the environment, so both are used up by the time this
    /// returns.
    fn enter(reply: protocol::AttachReply, fds: Vec<OwnedFd>) -> Result<()> {
        // setns(CLONE_NEWNS) resets the working directory to the new root when
        // the old one is not reachable there, so remember the caller's
        // directory before joining.
        let cwd = std::env::current_dir().ok();

        // Client::attach ensures there is one descriptor per namespace, so the
        // zip covers the whole list.
        for (fd, name) in fds.iter().zip(&reply.namespaces) {
            setns(fd, CloneFlags::empty())
                .with_context(|| format!("entering the {name} namespace"))?;
        }

        // The reply is applied to this process; execvp hands the result on to
        // the command. Modifying the environment is unsafe because another
        // thread could be reading it; this process is single-threaded and
        // about to exec.
        for (key, op) in &reply.env {
            if key.is_empty() {
                continue;
            }

            match op {
                protocol::EnvOp::Set(value) => unsafe { std::env::set_var(key, value) },
                protocol::EnvOp::Unset => unsafe { std::env::remove_var(key) },
            }
        }

        // Re-enter the caller's directory when it exists inside the sandbox.
        let home = std::env::var_os("HOME").unwrap_or_else(|| "/".into());
        let dir = cwd
            .filter(|path| path.is_dir())
            .unwrap_or_else(|| PathBuf::from(home));
        let _ = std::env::set_current_dir(&dir);

        // A session has at most one controlling terminal, and tcgetsid()
        // reports the session a tty belongs to: a descriptor carries ours
        // exactly when the two session ids agree.
        let sid = unsafe { libc::getsid(0) };
        for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
            if !sys::is_tty(fd) || unsafe { libc::tcgetsid(fd) } != sid {
                continue;
            }

            // The pty belongs to the host's devpts and the sandbox mounts its
            // own, so ttyname() fails with ENODEV although the descriptor
            // still works. /dev/console is a bind of /dev/tty, which resolves
            // to the terminal of whoever opens it; point the descriptor there
            // so the session has a name for it.
            if !unsafe { libc::ttyname(fd) }.is_null() {
                continue;
            }

            if let Ok(console) = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open("/dev/console")
            {
                unsafe { libc::dup2(console.as_raw_fd(), fd) };
            }
        }

        // The client has full capabilities in the sandbox user namespace
        // now. Nothing beyond this point needs them, and the exec'd
        // command must not inherit them.
        sys::set_no_new_privs().context("setting no_new_privs")?;
        sys::drop_all_caps().context("dropping capabilities")?;

        Ok(())
    }

    /// Fork the command and return its pid; the child never returns: it execs,
    /// or exits with the reason it could not.
    ///
    /// The command line is consumed here, so the parent does not keep a copy
    /// while it waits.
    fn spawn(argv: Vec<String>) -> Result<Pid> {
        let cargv: Vec<CString> = argv
            .iter()
            .map(|arg| {
                CString::new(arg.as_str())
                    .map_err(|_| anyhow::anyhow!("argument contains a NUL byte"))
            })
            .collect::<Result<_>>()?;

        if cargv.is_empty() {
            bail!("no command to run");
        }

        match unsafe { fork() }? {
            ForkResult::Child => {
                // Die with the client, so a killed sbox attach does not leave
                // the command running detached from the terminal.
                unsafe {
                    libc::prctl(
                        libc::PR_SET_PDEATHSIG,
                        libc::SIGKILL as libc::c_ulong,
                        0,
                        0,
                        0,
                    );
                }

                if getppid().as_raw() == 1 {
                    unsafe { libc::_exit(1) }
                }

                let _ = execvp(&cargv[0], &cargv);
                let err = std::io::Error::last_os_error();
                eprintln!("sbox: cannot execute {:?}: {}", argv[0], err);

                unsafe { libc::_exit(127) }
            }
            ForkResult::Parent { child } => Ok(child),
        }
    }

    /// Wait for the command and report its shell-style exit status.
    ///
    /// The whole process group receives the terminal signals; the client only
    /// has to survive long enough to report the status, so it ignores them.
    fn reap(child: Pid) -> Result<i32> {
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_IGN);
            libc::signal(libc::SIGQUIT, libc::SIG_IGN);
        }

        loop {
            match waitpid(child, None) {
                Ok(WaitStatus::Exited(_, code)) => return Ok(code),
                Ok(WaitStatus::Signaled(_, sig, _)) => return Ok(128 + sig as i32),
                Ok(_) => continue,
                Err(nix::errno::Errno::EINTR) => continue,
                Err(e) => return Err(e).context("waiting for the sandboxed command"),
            }
        }
    }
}
