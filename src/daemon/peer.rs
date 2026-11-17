//! Control socket peers and attached sessions for the daemon.
//!
//! A peer reads one control request and sends its reply. An Attach changes the
//! peer's role to a session while the same object continues to own the socket.
//! Request meaning belongs to the daemon, and so does epoll: a peer only
//! reports what its connection needs through `interest`.

use crate::protocol::{Request, RequestParser, Response};
use nix::sys::socket::{ControlMessage, MsgFlags, sendmsg};
use std::io::{IoSlice, Read};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use super::events::{EpollTag, TagKind};
use crate::linux::epoll::Source;

/// How long a client may take to say what it wants.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a reply may take to drain before the client is considered gone.
const RESPONSE_TIMEOUT: Duration = Duration::from_secs(5);

/// A frame on its way out.
///
/// The daemon keeps one of these per connection while the peer drains it, so a
/// client that stops reading cannot stall the event loop: the bytes wait here
/// and go out as EPOLLOUT reports room. The descriptors ride on the first
/// bytes that are actually written, so a retry after EAGAIN still delivers
/// them.
struct Outgoing {
    frame: Vec<u8>,
    offset: usize,
    fds: Vec<RawFd>,
}

impl Outgoing {
    fn new(frame: Vec<u8>, fds: &[RawFd]) -> Outgoing {
        Outgoing {
            frame,
            offset: 0,
            fds: fds.to_vec(),
        }
    }

    fn is_done(&self) -> bool {
        self.offset >= self.frame.len()
    }

    /// Write as much as the socket takes without waiting. Header and payload go
    /// out in one call, and the descriptors ride on the first write.
    fn write_some(&mut self, stream: &UnixStream) -> std::io::Result<()> {
        let sent = {
            let iov = [IoSlice::new(&self.frame[self.offset..])];
            // The descriptors ride on the first write; a zero offset means
            // nothing has been written yet.
            let cmsg: Vec<ControlMessage> = if self.fds.is_empty() || self.offset > 0 {
                Vec::new()
            } else {
                vec![ControlMessage::ScmRights(&self.fds)]
            };

            sendmsg::<()>(
                stream.as_raw_fd(),
                &iov,
                &cmsg,
                MsgFlags::MSG_NOSIGNAL,
                None,
            )
            .map_err(std::io::Error::from)?
        };

        if sent == 0 {
            return Err(std::io::ErrorKind::WriteZero.into());
        }

        self.offset += sent;

        Ok(())
    }

    /// Push the frame out. Blocked means the socket is full, and only EPOLLOUT
    /// can clear that.
    fn flush(&mut self, stream: &UnixStream) -> Flush {
        while !self.is_done() {
            match self.write_some(stream) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Flush::Blocked,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Flush::Dead,
            }
        }

        Flush::Done
    }
}

/// How far a reply got.
enum Flush {
    /// The whole frame is out.
    Done,
    /// The socket buffer is full; wait for EPOLLOUT.
    Blocked,
    /// The connection is gone.
    Dead,
}

/// One control connection, from its first request through its session lifetime.
pub(super) struct Peer {
    stream: UnixStream,
    /// The bytes of the request being read. A connection reads exactly one.
    parser: RequestParser,
    role: Role,
    state: State,
}

/// Whether the connection is still a control peer or has been attached as a session.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Role {
    Peer,
    Session,
}

/// What the connection is currently doing.
enum State {
    Reading {
        deadline: Instant,
    },
    Writing {
        out: Outgoing,
        deadline: Instant,
    },
    Connected,
    /// Nothing more will arrive from or go to this connection.
    Closed,
}

/// What one event asked the daemon to do.
pub(super) enum Event {
    /// There is nothing to answer yet.
    Pending,
    /// A complete request is ready to be answered.
    Request(Request),
    /// The connection is over; the daemon drops it.
    Ended,
}

impl Peer {
    /// A newly accepted connection that has not sent a request.
    pub fn new(stream: UnixStream) -> Peer {
        Peer {
            stream,
            parser: RequestParser::new(),
            role: Role::Peer,
            state: State::Reading {
                deadline: Instant::now() + REQUEST_TIMEOUT,
            },
        }
    }

    pub fn deadline(&self) -> Option<Instant> {
        match &self.state {
            State::Reading { deadline } | State::Writing { deadline, .. } => Some(*deadline),
            State::Connected | State::Closed => None,
        }
    }

    /// Mark an accepted Attach connection as a session before sending its reply.
    pub fn become_session(&mut self) {
        debug_assert!(matches!(self.state, State::Reading { .. }));
        self.role = Role::Session;
    }

    pub fn client_pid(&self) -> Option<u32> {
        if self.role != Role::Session {
            return None;
        }

        nix::sys::socket::getsockopt(&self.stream, nix::sys::socket::sockopt::PeerCredentials)
            .ok()
            .map(|credentials| credentials.pid() as u32)
    }

    /// What epoll should watch for this connection, or `None` once it has
    /// nothing left to wait for.
    ///
    /// A request is arriving, a reply is draining, or an attached session sits
    /// idle; each waits for a different readiness, and an idle session only has
    /// to learn when its client is gone.
    pub fn interest(&self) -> Option<i32> {
        match &self.state {
            State::Reading { .. } => Some(libc::EPOLLIN | libc::EPOLLRDHUP),
            State::Writing { .. } => Some(libc::EPOLLOUT),
            State::Connected => Some(libc::EPOLLRDHUP),
            State::Closed => None,
        }
    }

    /// Handle one readiness event.
    ///
    /// The event decides what happens before the socket is touched: a readable
    /// request is read, a writable reply is flushed, and a hang-up ends the
    /// connection. Whatever the transition leaves behind is what `interest`
    /// reports.
    pub fn on_event(&mut self, events: u32) -> Event {
        let events = events as i32;

        // Whatever the connection was doing, an error or a full hang-up leaves
        // nothing left to wait for.
        if events & (libc::EPOLLERR | libc::EPOLLHUP) != 0 {
            return self.close();
        }

        match self.state {
            // Read whenever there may be bytes or the client has closed its
            // side: read() reports the end of the stream as an empty read.
            State::Reading { .. } if events & (libc::EPOLLIN | libc::EPOLLRDHUP) != 0 => {
                self.read()
            }
            State::Reading { .. } => Event::Pending,
            State::Writing { .. } if events & libc::EPOLLOUT != 0 => self.flush(),
            State::Writing { .. } => Event::Pending,
            // A session registers EPOLLRDHUP and nothing else, so any event
            // that reaches it means the client is gone.
            State::Connected => self.close(),
            State::Closed => Event::Ended,
        }
    }

    /// Send a reply. Peers close when it completes; sessions go idle. What the
    /// attempt leaves behind is what `interest` reports.
    pub fn reply(&mut self, response: Response, fds: &[RawFd]) {
        let frame = match response.encode() {
            Ok(frame) => frame,
            Err(e) => {
                eprintln!("sbox: cannot encode a reply: {e:#}");
                self.close();
                return;
            }
        };

        self.state = State::Writing {
            out: Outgoing::new(frame, fds),
            deadline: Instant::now() + RESPONSE_TIMEOUT,
        };

        self.flush();
    }

    /// Read whatever has arrived without blocking and feed it to the parser.
    fn read(&mut self) -> Event {
        loop {
            match self.stream.read(self.parser.spare_mut()) {
                Ok(0) => return self.close(),
                Ok(n) => match self.parser.commit(n) {
                    Ok(Some(request)) => return Event::Request(request),
                    Ok(None) => {}
                    // The bytes are not a request this sandbox knows: say so,
                    // then let the connection close.
                    Err(e) => {
                        self.reply(Response::Error(format!("{e:#}")), &[]);
                        return match self.state {
                            State::Closed => Event::Ended,
                            _ => Event::Pending,
                        };
                    }
                },
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Event::Pending,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return self.close(),
            }
        }
    }

    /// Push the reply out. A control peer is done once its reply is out, a
    /// session goes idle, and a dead socket closes.
    fn flush(&mut self) -> Event {
        let State::Writing { out, .. } = &mut self.state else {
            return Event::Pending;
        };

        match out.flush(&self.stream) {
            Flush::Dead => self.close(),
            Flush::Blocked => Event::Pending,
            Flush::Done => match self.role {
                Role::Peer => self.close(),
                Role::Session => {
                    self.state = State::Connected;
                    Event::Pending
                }
            },
        }
    }

    /// Close the connection: nothing more will arrive from or go to it.
    fn close(&mut self) -> Event {
        self.state = State::Closed;
        Event::Ended
    }
}

impl Source for Peer {
    fn fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    fn token(&self) -> u64 {
        EpollTag::token(TagKind::Peer, self.stream.as_raw_fd())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Squeeze the send buffer so a sizeable frame cannot leave in one call.
    fn shrink_send_buffer(stream: &UnixStream, size: libc::c_int) {
        let rc = unsafe {
            libc::setsockopt(
                stream.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_SNDBUF,
                &size as *const _ as *const libc::c_void,
                std::mem::size_of::<libc::c_int>() as libc::socklen_t,
            )
        };
        assert_eq!(rc, 0, "setsockopt: {}", std::io::Error::last_os_error());
    }

    /// Write until the socket refuses, and say whether it did.
    fn fill(stream: &UnixStream, out: &mut Outgoing) -> bool {
        while !out.is_done() {
            match out.write_some(stream) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return true,
                Err(e) => panic!("write_some: {e}"),
            }
        }
        false
    }

    /// Read everything that has arrived, without blocking.
    fn drain(stream: &mut UnixStream) -> usize {
        let mut total = 0;
        loop {
            let mut sink = [0u8; 8192];
            match stream.read(&mut sink) {
                Ok(0) => return total,
                Ok(n) => total += n,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return total,
                Err(e) => panic!("read: {e}"),
            }
        }
    }

    #[test]
    fn a_large_frame_goes_out_in_instalments() {
        let (writer, mut reader) = UnixStream::pair().expect("socketpair");
        shrink_send_buffer(&writer, 4096);
        writer.set_nonblocking(true).expect("nonblocking");
        reader.set_nonblocking(true).expect("nonblocking");

        let frame = Response::Error("x".repeat(200_000))
            .encode()
            .expect("encode");
        let expected = frame.len();
        let mut out = Outgoing::new(frame, &[]);

        // Nobody is reading, so the send buffer has to fill up.
        assert!(
            fill(&writer, &mut out),
            "a 200 KiB frame fit in a 4 KiB send buffer"
        );

        // Draining the peer lets the rest through, a piece at a time.
        let mut drained = 0;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !out.is_done() {
            assert!(
                std::time::Instant::now() < deadline,
                "the frame never drained"
            );
            match out.write_some(&writer) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                Err(e) => panic!("write_some: {e}"),
            }
            drained += drain(&mut reader);
        }
        drained += drain(&mut reader);

        assert_eq!(drained, expected);
    }

    #[test]
    fn a_request_is_read_and_a_peer_closes_once_answered() {
        let (mut client, server) = UnixStream::pair().expect("socketpair");
        server.set_nonblocking(true).expect("nonblocking");

        let mut peer = Peer::new(server);
        assert_eq!(peer.interest(), Some(libc::EPOLLIN | libc::EPOLLRDHUP));

        // Nothing has arrived, so the readable event has nothing to read.
        assert!(matches!(
            peer.on_event(libc::EPOLLIN as u32),
            Event::Pending
        ));

        client.write_all(&Request::Status.encode()).expect("write");

        assert!(matches!(
            peer.on_event(libc::EPOLLIN as u32),
            Event::Request(Request::Status)
        ));

        // A plain peer has nothing left to wait for once its reply is out.
        peer.reply(Response::Stop, &[]);
        assert_eq!(peer.interest(), None);
    }

    #[test]
    fn an_attached_session_goes_idle_after_its_reply() {
        let (mut client, server) = UnixStream::pair().expect("socketpair");
        server.set_nonblocking(true).expect("nonblocking");

        client.write_all(&Request::Attach.encode()).expect("write");
        let mut peer = Peer::new(server);
        assert!(matches!(
            peer.on_event(libc::EPOLLIN as u32),
            Event::Request(Request::Attach)
        ));

        peer.become_session();
        peer.reply(Response::Stop, &[]);
        assert_eq!(peer.interest(), Some(libc::EPOLLRDHUP));

        // A session registers only for the client going away.
        drop(client);
        assert!(matches!(
            peer.on_event(libc::EPOLLRDHUP as u32),
            Event::Ended
        ));
        assert_eq!(peer.interest(), None);
    }
}
