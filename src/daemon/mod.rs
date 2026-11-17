//! The sandbox daemon: PID 1 of the sandbox and the only process that talks to
//! the control socket.
//!
//! It hands out the namespace descriptors a client needs to join, answers
//! status queries, watches the sessions, reaps orphans, and retires itself when
//! its sessions and accepted connections end (unless the sandbox was asked to
//! stay).

use self::events::{EpollTag, TagKind};
use crate::config::CONTROL_SOCKET;
use crate::linux::epoll::Epoll;
use crate::linux::{fs, sys};
use crate::name::Name;
use crate::protocol::{AttachReply, Request, Response, StatusReply};
use anyhow::{Context, Result};
use std::collections::BTreeMap;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::time::Instant;

mod events;
mod peer;
use peer::{Event, Peer};

/// What setup built on its way to becoming the sandbox: the descriptors a
/// session joins, and the environment it starts from.
pub(crate) struct Entered {
    pub fds: Vec<OwnedFd>,
    pub reply: AttachReply,
}

/// Everything setup has to hand the daemon: who the sandbox is, the listener
/// it answers on, the host-side resources it releases on retire, and whether
/// it outlives its sessions. This is built before the daemon exists, which is
/// what makes it a value of its own rather than a handful of Daemon fields.
pub(crate) struct Runtime {
    pub name: Name,
    /// The sandbox directory, as it was when the sandbox was created.
    pub dir: PathBuf,
    /// Host pid of this process. The daemon is PID 1 in its own pid namespace,
    /// so getpid() cannot tell it; the parent passes the value it got from
    /// fork() through a pipe.
    pub host_pid: i32,
    /// Creation time, in seconds since the epoch.
    pub started: u64,

    pub listener: UnixListener,
    /// Whether the sandbox outlives its last session (--keep, --no-attach).
    pub persistent: bool,

    // Host-side resources that keep the sandbox alive across pivot_root().
    // They are declared last so that dropping the daemon releases them in
    // this order: the proxy stops, the socket directory handle closes, and
    // the name lock is released.
    /// Dropping it stops xdg-dbus-proxy.
    #[allow(dead_code)]
    pub dbus_proxy: Option<OwnedFd>,
    /// The directory the control socket lives in.
    pub sock_dir: fs::FsNode,
    /// The flock that marks the sandbox name as taken. Dropping the file
    /// releases it, which is what makes the name available again.
    #[allow(dead_code)]
    pub lock: std::fs::File,
}

/// The one persistent process per sandbox.
pub(crate) struct Daemon {
    runtime: Runtime,
    /// What the setup process built on its way to becoming the sandbox: the
    /// descriptors a session joins, and the environment it starts from.
    entered: Entered,

    epoll: Epoll,

    signals: sys::SignalFd,

    /// The open connections, keyed by the descriptor they are registered under.
    connections: BTreeMap<RawFd, Peer>,
    saw_session: bool,
    /// Set when a client or a signal asks the sandbox to retire.
    stopping: bool,
}

impl Daemon {
    /// Serve until the sandbox retires. Never returns: the daemon leaves
    /// through _exit(), because its client has already gone its own way.
    pub fn start(runtime: Runtime, entered: Entered) -> ! {
        let status = match Daemon::new(runtime, entered) {
            Ok(daemon) => daemon.run(),
            Err(e) => {
                eprintln!("sbox: {e:#}");
                1
            }
        };
        unsafe { libc::_exit(status) }
    }

    fn new(runtime: Runtime, entered: Entered) -> Result<Daemon> {
        let epoll = Epoll::new()?;

        let signals = sys::SignalFd::new(&[libc::SIGTERM, libc::SIGCHLD])?;
        epoll.add(&signals, libc::EPOLLIN)?;

        runtime
            .listener
            .set_nonblocking(true)
            .context("setting the control socket non-blocking")?;
        epoll.add(&runtime.listener, libc::EPOLLIN)?;

        Ok(Daemon {
            runtime,
            entered,
            epoll,
            signals,
            connections: BTreeMap::new(),
            saw_session: false,
            stopping: false,
        })
    }

    /// Serve until the sandbox retires, and report the status the process
    /// should leave behind.
    fn run(mut self) -> i32 {
        loop {
            let mut events = [libc::epoll_event { events: 0, u64: 0 }; 64];
            let n = match self.epoll.wait(&mut events, self.timeout()) {
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    let error = anyhow::Error::new(e).context("epoll_wait");
                    eprintln!("sbox: {error:#}");
                    return 1;
                }
            };

            for event in &events[..n] {
                let Some(tag) = EpollTag::decode(event.u64) else {
                    debug_assert!(false, "epoll returned a tag this daemon never registered");
                    continue;
                };

                match tag.kind() {
                    TagKind::Listener => self.accept(),
                    TagKind::Signals => self.signalled(),
                    TagKind::Peer => self.peer_io(tag, event.events),
                }
            }

            let now = Instant::now();
            // Stalled requests and replies expire; connected sessions have no deadline.
            self.connections
                .retain(|_, peer| peer.deadline().is_none_or(|deadline| deadline > now));

            reap();

            let idle = !self.runtime.persistent && self.saw_session && self.connections.is_empty();

            if self.stopping || idle {
                // Nothing to report to: the socket may already be gone, and this
                // process is on its way out either way.
                let _ = self
                    .runtime
                    .sock_dir
                    .as_dir()
                    .unlink(Path::new(CONTROL_SOCKET));

                // Dropping the daemon stops the proxy and releases the name.
                drop(self);
                return 0;
            }
        }
    }

    /// How long epoll may sleep: until the next connection deadline. No pending
    /// deadline means forever.
    fn timeout(&self) -> i32 {
        match self.connections.values().filter_map(Peer::deadline).min() {
            None => -1,
            Some(deadline) => deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .min(i32::MAX as u128) as i32,
        }
    }

    /// Every connection starts out as a probe: the client has to send a request
    /// frame before anything happens. Only an attach becomes a session, so a
    /// status query can neither keep a sandbox alive nor take a namespace
    /// descriptor away.
    fn accept(&mut self) {
        loop {
            match self.runtime.listener.accept() {
                Ok((stream, _)) => {
                    if let Err(e) = stream.set_nonblocking(true) {
                        eprintln!("sbox: setting the request socket non-blocking: {e}");
                        continue;
                    }

                    let fd = stream.as_raw_fd();
                    let peer = Peer::new(stream);

                    // A new peer is always reading, so it has an interest to
                    // register; one that is closed has nothing to join.
                    let Some(interest) = peer.interest() else {
                        continue;
                    };
                    if let Err(e) = self.epoll.add(&peer, interest) {
                        eprintln!("sbox: epoll_ctl: {e}");
                        continue;
                    }

                    self.connections.insert(fd, peer);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    eprintln!("sbox: accept: {e}");
                    break;
                }
            }
        }
    }

    /// Note a signal that asks the sandbox to retire.
    fn signalled(&mut self) {
        if self
            .signals
            .read()
            .into_iter()
            .any(|signal| signal == libc::SIGTERM)
        {
            self.stopping = true;
        }
    }

    /// Handle one connection event.
    fn peer_io(&mut self, tag: EpollTag, events: u32) {
        let fd = tag.fd();

        let Some(peer) = self.connections.get_mut(&fd) else {
            return;
        };

        // The peer reads or drains what the event announced. The borrow ends
        // here: answering a request reads the rest of the daemon, and a status
        // query reads every connection.
        let event = peer.on_event(events);

        match event {
            Event::Ended => {
                // Dropping the peer closes its socket and unregisters it.
                self.connections.remove(&fd);
                return;
            }
            // Nothing arrived to answer; only the interest may have changed.
            Event::Pending => {}
            Event::Request(Request::Attach) => {
                self.saw_session = true;

                let fds: Vec<RawFd> = self.entered.fds.iter().map(AsRawFd::as_raw_fd).collect();
                let response = Response::Attach(self.entered.reply.clone());

                peer.become_session();
                peer.reply(response, &fds);
            }
            Event::Request(Request::Status) => {
                let response = Response::Status(self.status_reply());

                if let Some(peer) = self.connections.get_mut(&fd) {
                    peer.reply(response, &[]);
                }
            }
            Event::Request(Request::Stop) => {
                self.stopping = true;

                peer.reply(Response::Stop, &[]);
            }
        }

        let Some(peer) = self.connections.get(&fd) else {
            return;
        };

        if !self.watch(peer) {
            self.connections.remove(&fd);
        }
    }

    /// Point epoll at what the peer needs now. Returns false when the
    /// connection has nothing left to wait for, or cannot be watched.
    fn watch(&self, peer: &Peer) -> bool {
        let Some(interest) = peer.interest() else {
            return false;
        };

        match self.epoll.modify(peer, interest) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("sbox: epoll_ctl: {e}");
                false
            }
        }
    }

    /// The state a status query asks for. The pid is the host pid the parent
    /// reported: this process is PID 1 in its own pid namespace and cannot
    /// learn its own.
    fn status_reply(&self) -> StatusReply {
        StatusReply {
            dir: self.runtime.dir.display().to_string(),
            pid: self.runtime.host_pid,
            started: self.runtime.started,
            sessions: self
                .connections
                .values()
                .filter_map(Peer::client_pid)
                .collect(),
        }
    }
}

fn reap() {
    loop {
        match nix::sys::wait::waitpid(None, Some(nix::sys::wait::WaitPidFlag::WNOHANG)) {
            Ok(nix::sys::wait::WaitStatus::StillAlive) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
}
