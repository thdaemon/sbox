//! Host-side setup process: claims the sandbox name, enters the namespaces and
//! leaves the daemon behind.

use crate::config::{Config, Hostname};
use crate::daemon::{self, Entered, Runtime};
use crate::linux::{fs, sys};
use crate::protocol::AttachReply;
use crate::sandbox::dbus_proxy::{BusSpec, DbusProxy, DbusProxySockets};
use crate::sandbox::environment::{HostEnv, SandboxEnv};
use crate::sandbox::mount::Builder;
use anyhow::{Context, Result};
use nix::sched::{CloneFlags, unshare};
use nix::unistd::{ForkResult, Pid, fork, sethostname};
use std::fs::File;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

use super::readiness::{Fault, Readiness};

/// The short-lived setup process: everything that needs host paths, the name,
/// and the namespaces. It takes the name, publishes the control socket, starts
/// the dbus proxies, builds the namespaces, and leaves one persistent process
/// behind. Only then does it leave the caller terminal.
pub(super) struct Setup {
    config: Config,
}

impl Setup {
    pub fn new(config: Config) -> Setup {
        Setup { config }
    }

    /// Start the sandbox, reporting the outcome through the readiness pipe.
    /// Never returns: success ends in the daemon, failure in _exit.
    pub fn run(self, readiness: Readiness) -> ! {
        let prepared = match self.prepare() {
            Ok(prepared) => prepared,
            Err(fault) => Self::report(fault, readiness),
        };

        match unsafe { fork() } {
            Ok(ForkResult::Child) => self.become_daemon(prepared, readiness),
            Ok(ForkResult::Parent { child }) => prepared.report_pid(child),
            Err(err) => Self::report(Fault::Failed(err.into()), readiness),
        }
    }

    /// Report why the sandbox did not start, then leave. A busy name is not a
    /// failure: the caller attaches to the sandbox that owns it.
    fn report(fault: Fault, readiness: Readiness) -> ! {
        let code = match fault {
            Fault::Busy => {
                readiness.busy();
                0
            }
            Fault::Failed(err) => {
                readiness.failed();
                eprintln!("sbox: {err:#}");
                1
            }
        };

        unsafe { libc::_exit(code) }
    }

    /// Everything that must happen once, on the host, before the process
    /// splits: the name, the control socket, the proxies, the namespaces, and
    /// the pipe the child reports its host pid on.
    fn prepare(&self) -> Result<Prepared, Fault> {
        // The daemon log is a host path, so it is opened before the mount tree.
        // The switch to it waits until the sandbox is up (see the child below),
        // so everything that fails during setup prints on the caller's terminal.
        let output = Output::open(&self.config.state_dir());

        let host = HostEnv::snapshot();

        let lock = self.take_lock()?;
        let listener = self.publish_socket()?;

        let dirs = self.config.dirs();
        let dbus = &self.config.sandbox.dbus;

        let (dbus_session, dbus_system) = if dbus.raw {
            (None, None)
        } else {
            (host.dbus_session.clone(), host.dbus_system_bus())
        };

        let (dbus_proxy_sockets, dbus_proxy_guard) = DbusProxy::new()
            .debug(dbus.debug)
            .log(
                dbus.debug
                    .then(|| self.config.state_dir().join("xdg-dbus-proxy-debug.log")),
            )
            .session(dbus_session.map(|address| {
                BusSpec::new(
                    address,
                    dirs.dbus_proxy_session_socket(self.config.name()),
                    dbus.session_rules(),
                )
            }))
            .system(dbus_system.map(|address| {
                BusSpec::new(
                    address,
                    dirs.dbus_proxy_system_socket(self.config.name()),
                    dbus.system_rules(),
                )
            }))
            .start()?;

        if self.config.verbose() {
            eprintln!(
                "sbox: starting sandbox '{}' in {}",
                self.config.name(),
                self.config.sandbox_dir().display()
            );
        }

        // The host identity has to be captured before the first user
        // namespace: inside it the process already shows up as uid 0.
        let host_uid = nix::unistd::Uid::current().as_raw();
        let host_gid = nix::unistd::Gid::current().as_raw();

        // The first level is a user namespace that maps root onto the caller,
        // plus a pid namespace. Both are unshared before the fork, so the
        // forked child becomes PID 1. Mounts then happen where uid 0 is
        // mapped, and the mount namespace belongs to a user namespace the
        // sandbox itself never enters: it holds no capabilities there, even if
        // it regains CAP_SYS_ADMIN in its own namespace.
        unshare(CloneFlags::CLONE_NEWUSER).context("creating the first user namespace")?;
        sys::setup_id_map(0, host_uid, 0, host_gid)?;
        unshare(CloneFlags::CLONE_NEWPID).context("creating the pid namespace")?;

        // Directory handles, so the sandbox can clean up after pivot_root().
        let sock_dir_path = self.config.dirs().socket_dir(self.config.name());
        let sock_dir = fs::FsNode::open(
            &sock_dir_path,
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
            0,
        )
        .with_context(|| format!("opening {}", sock_dir_path.display()))?;
        let started = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        // The daemon is PID 1 of the sandbox pid namespace, so getpid() there
        // returns 1; only the parent learns the host pid, from fork(). The
        // value travels through a pipe, because nothing else survives the
        // fork with it.
        let (pid_read, pid_write) =
            nix::unistd::pipe2(nix::fcntl::OFlag::O_CLOEXEC).context("pipe2")?;

        Ok(Prepared {
            output,
            host,
            host_uid,
            host_gid,
            lock,
            listener,
            dbus_proxy_sockets,
            dbus_proxy_guard,
            sock_dir,
            started,
            pid_read,
            pid_write,
        })
    }

    /// The forked child: PID 1 of the sandbox from here on. It never returns,
    /// so nothing is dropped unless it is dropped explicitly.
    fn become_daemon(self, prepared: Prepared, readiness: Readiness) -> ! {
        let Prepared {
            output,
            host,
            host_uid,
            host_gid,
            lock,
            listener,
            dbus_proxy_sockets,
            dbus_proxy_guard,
            sock_dir,
            started,
            pid_read,
            pid_write,
        } = prepared;

        // Only the parent learns the host pid, from fork(); the value travels
        // through the pipe because nothing else survives the fork with it.
        drop(pid_write);

        let Some(host_pid) = read_pid(pid_read.as_fd()) else {
            readiness.failed();
            eprintln!("sbox: the parent did not report our pid");
            unsafe { libc::_exit(1) }
        };
        drop(pid_read);

        let runtime = Runtime {
            name: self.config.name().clone(),
            dir: self.config.sandbox_dir(),
            host_pid,
            started,
            listener,
            persistent: self.config.persistent(),
            dbus_proxy: dbus_proxy_guard,
            sock_dir,
            lock,
        };

        // The sandbox process inherits the caller's command line, so without
        // this ps(1) and top(1) would show another sbox invocation. The name
        // is what tells the daemons of different sandboxes apart.
        let _ = sys::set_process_name(&format!("sbox:{}", runtime.name));

        // A failure here has nobody to report to but the readiness pipe, so it
        // ends the process.
        let entered = match self.enter(&dbus_proxy_sockets, &host, host_uid, host_gid) {
            Ok(entered) => entered,
            Err(e) => {
                eprintln!("sbox: {e:#}");
                unsafe { libc::_exit(1) }
            }
        };

        // Everything that can fail has run. From here on this process is the
        // sandbox and must not keep the caller's terminal.
        output.detach();

        readiness.ready();

        daemon::Daemon::start(runtime, entered)
    }

    /// Everything that needs privileges: the namespaces, the mount tree, the
    /// switch into the sandbox's own user namespace, and dropping what made
    /// that possible.
    ///
    /// Runs in the forked child. The first level and the pid namespace are
    /// already in place by then: unshare(CLONE_NEWPID) only affects future
    /// children, which is why it belongs before the fork, in prepare().
    fn enter(
        &self,
        proxies: &DbusProxySockets,
        host: &HostEnv,
        host_uid: u32,
        host_gid: u32,
    ) -> Result<Entered> {
        let hostname = self.hostname();
        // Some(name) when the sandbox owns its uts namespace. The same value
        // decides whether uts is unshared and whether it is handed out, so the
        // two cannot disagree.
        let uts = hostname.name();

        // The environment is built in one place: the baseline, then what the
        // mount tree records as it shares each host path, then the user's
        // configuration.
        let mut env = SandboxEnv::new();

        // Every namespace is recorded as it is opened: the descriptor and the
        // name a session joins it by come from the same statement, so nothing
        // has to keep two lists in step. The order of these statements is the
        // order the session joins them in.
        let mut namespaces: Vec<(&'static str, OwnedFd)> =
            vec![("user", sys::open_namespace("user")?)];

        // The sandbox gets its own mount tree first; the user namespace it
        // actually sees comes after the mounts are in place.
        let mut unshare_flags = CloneFlags::CLONE_NEWNS | CloneFlags::CLONE_NEWIPC;
        if uts.is_some() {
            unshare_flags |= CloneFlags::CLONE_NEWUTS;
        }
        unshare(unshare_flags).context("creating sandbox namespaces")?;
        // From here on, mounts made while building the sandbox stay inside it.
        fs::FsMount::open_from(Path::new("/"))?.make_slave()?;

        if let Some(name) = uts {
            sethostname(name).context("sethostname")?;
        }

        namespaces.push(("mnt", sys::open_namespace("mnt")?));
        if uts.is_some() {
            namespaces.push(("uts", sys::open_namespace("uts")?));
        }
        namespaces.push(("ipc", sys::open_namespace("ipc")?));
        namespaces.push(("pid", sys::open_namespace("pid")?));

        Builder::build(&self.config, host, proxies, &mut env).context("sandbox setup failed")?;

        let (uid, gid) = if self.config.sandbox.fakeroot {
            (0, 0)
        } else {
            (host_uid, host_gid)
        };

        // The second level is the user namespace the sandbox itself sees: the
        // ids map onto uid 0 of the first level, which resolves back to the
        // caller. Unless nested is set this also spends the level-one budget
        // and verifies that no further user namespace can be created, the way
        // bubblewrap's --disable-userns does.
        let nested = self.config.sandbox.nested;
        if !nested {
            sys::limit_user_namespaces(1).context("user.max_user_namespaces = 1")?;
        }

        unshare(CloneFlags::CLONE_NEWUSER).context("creating the second user namespace")?;
        sys::setup_id_map(uid, 0, gid, 0)?;

        if !nested {
            sys::assert_userns_disabled()?;
        }

        namespaces.push(("user", sys::open_namespace("user")?));

        sys::set_no_new_privs().context("setting no_new_privs")?;
        sys::drop_all_caps().context("dropping capabilities")?;
        // The initializer is the only process in the sandbox that never execs,
        // so keep it non-dumpable: same-uid processes cannot ptrace it or read
        // /proc/1/{mem,fd,environ} without CAP_SYS_PTRACE, which nothing inside
        // the sandbox has. Commands exec and become dumpable on their own.
        if let Err(e) = sys::set_dumpable(false) {
            eprintln!("sbox: cannot make the initializer non-dumpable: {e:#}");
        }

        env.configure(&self.config);

        let names = namespaces
            .iter()
            .map(|(kind, _)| kind.to_string())
            .collect();
        let reply = AttachReply {
            env: env.into_env(),
            namespaces: names,
        };

        let fds = namespaces.into_iter().map(|(_, fd)| fd).collect();

        Ok(Entered { fds, reply })
    }

    /// The hostname the sandbox will have. Auto means the name derived from the
    /// host and the sandbox, resolved here because only this layer knows the
    /// host's name is still visible.
    fn hostname(&self) -> Hostname {
        let host = nix::unistd::gethostname()
            .map(|host| host.to_string_lossy().into_owned())
            .unwrap_or_else(|_| "localhost".into());
        self.config
            .sandbox
            .hostname
            .resolve(&host, self.config.name())
    }

    /// Take the name, or report that someone else has it. Everything that
    /// identifies a sandbox -- the control socket, the state directory, the
    /// lock -- is keyed by the name, so the lock lives in the directory the
    /// name keys.
    fn take_lock(&self) -> Result<std::fs::File, Fault> {
        let dirs = self.config.dirs();
        let root = dirs.socket_root();
        let dir = dirs.socket_dir(self.config.name());

        // The runtime directories hold the control socket, so they must not be
        // world-accessible even when something else created them first.
        for path in [&root, &dir] {
            std::fs::create_dir_all(path).with_context(|| format!("mkdir {}", path.display()))?;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
                .with_context(|| format!("chmod 0700 {}", path.display()))?;
        }

        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dirs.lock_file(self.config.name()))
            .context("opening the lock")?;

        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(Fault::Busy);
        }
        Ok(lock)
    }

    /// Publish the control socket. Holding the lock means no other sbox may own
    /// this name, so a socket left at the path is from a crashed daemon and can
    /// be replaced; one that answers belongs to a daemon started by an older
    /// sbox, which locked the sandbox directory instead.
    fn publish_socket(&self) -> Result<UnixListener, Fault> {
        let path = self.config.dirs().control_socket(self.config.name());
        if path.exists() {
            match UnixStream::connect(&path) {
                Ok(_) => return Err(Fault::Busy),
                Err(_) => {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }

        let listener = UnixListener::bind(&path)
            .with_context(|| format!("binding {}", path.display()))
            .map_err(Fault::Failed)?;

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
            .with_context(|| format!("chmod 0600 {}", path.display()))?;

        Ok(listener)
    }
}

/// Everything the setup process has to carry across the fork: the host-side
/// resources the child turns into the sandbox, and the pipe it reports its
/// host pid on.
struct Prepared {
    output: Output,
    host: HostEnv,
    host_uid: u32,
    host_gid: u32,
    lock: std::fs::File,
    listener: UnixListener,
    dbus_proxy_sockets: DbusProxySockets,
    dbus_proxy_guard: Option<OwnedFd>,
    sock_dir: fs::FsNode,
    started: u64,
    pid_read: OwnedFd,
    pid_write: OwnedFd,
}

impl Prepared {
    /// Hand the forked child its host pid and leave. Nothing is dropped here:
    /// _exit closes the descriptors without running their destructors, so the
    /// parent cannot tear down the listener, the proxy guard or the name lock
    /// on its way out.
    fn report_pid(self, child: Pid) -> ! {
        let bytes = child.as_raw().to_ne_bytes();
        if let Err(e) = sys::write_all(self.pid_write.as_raw_fd(), &bytes) {
            // The child is already blocked on the other end; it will see the
            // closed pipe and report its own failure.
            eprintln!("sbox: reporting the sandbox pid: {e}");
        }

        unsafe { libc::_exit(0) }
    }
}

/// Where the daemon's output goes once setup leaves the caller terminal.
///
/// The daemon log is a host path, so it is opened before the mount tree is
/// built. The switch itself happens as late as possible, so a failure while
/// creating the sandbox still reaches the caller's terminal.
struct Output {
    null: Option<File>,
    daemon: Option<File>,
}

impl Output {
    fn open(state_dir: &Path) -> Output {
        let null = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")
            .ok();
        let daemon = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(state_dir.join("daemon.log"))
            .ok();
        Output { null, daemon }
    }

    /// Leave the caller's terminal: stdin from /dev/null, stdout and stderr to
    /// the daemon log from now on.
    fn detach(&self) {
        if let Some(null) = &self.null {
            unsafe {
                libc::dup2(null.as_raw_fd(), 0);
            }
        }
        if let Some(daemon) = &self.daemon {
            unsafe {
                libc::dup2(daemon.as_raw_fd(), 1);
                libc::dup2(daemon.as_raw_fd(), 2);
            }
        }
    }
}

fn read_pid(fd: BorrowedFd<'_>) -> Option<i32> {
    let mut buf = [0u8; std::mem::size_of::<libc::pid_t>()];
    sys::read_full(fd.as_raw_fd(), &mut buf).ok()?;
    Some(i32::from_ne_bytes(buf))
}
