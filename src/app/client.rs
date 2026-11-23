// SPDX-FileCopyrightText: 2026 Eric Tian <thxdaemon@gmail.com>
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The client side of the control socket.
//!
//! It sends one request, reads one reply and hands the attach descriptors back
//! to the caller. The daemon side lives in daemon; protocol.rs only turns
//! messages into bytes.

use crate::name::Name;
use crate::protocol::{AttachReply, Request, Response, ResponseParser, StatusReply};
use anyhow::{Context, Result, bail};
use nix::sys::socket::{ControlMessageOwned, MsgFlags, recvmsg};
use std::io::{IoSliceMut, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// How often to look again while the daemon is still publishing its socket.
const CONNECT_INTERVAL: Duration = Duration::from_millis(20);

/// Why the control socket could not be reached. Only the first is worth
/// retrying: the sandbox is still coming up.
#[derive(Debug)]
pub(super) enum ConnectError {
    NotRunning,
    Io(std::io::Error),
}

impl ConnectError {
    pub fn describe(self, name: &Name) -> anyhow::Error {
        match self {
            ConnectError::NotRunning => anyhow::anyhow!("sandbox '{name}' is not running"),
            ConnectError::Io(e) => {
                anyhow::Error::new(e).context(format!("connecting to sandbox '{name}'"))
            }
        }
    }
}

/// A client side of the control socket: one request, one reply, decoded.
///
/// Every method names the exchange it performs, so call sites never deal with
/// action codes, empty payloads or reply frames. The client keeps the stream
/// because attach hands it out as the session lease.
pub(super) struct Client {
    stream: UnixStream,
}

/// An answered attach: the descriptors to join and the environment to start
/// from. The lease is the connection itself and must outlive the session.
#[derive(Debug)]
pub(super) struct Attached {
    pub lease: UnixStream,
    pub reply: AttachReply,
    pub fds: Vec<OwnedFd>,
}

impl Client {
    /// Wrap a connected control socket and put a time bound on every reply read.
    fn new(stream: UnixStream, read_timeout: Duration) -> std::io::Result<Client> {
        stream.set_read_timeout(Some(read_timeout))?;
        Ok(Client { stream })
    }

    /// Open a control socket, waiting up to `patience` for the daemon to
    /// publish it, and put a time bound on every reply read. Zero patience
    /// means "answer now", which is what status and stop want; starting a
    /// sandbox and connecting to it are two steps, so a caller that just asked
    /// for one has to wait.
    pub fn connect(
        path: PathBuf,
        patience: Duration,
        read_timeout: Duration,
    ) -> Result<Client, ConnectError> {
        let deadline = Instant::now() + patience;

        loop {
            match UnixStream::connect(&path) {
                Ok(stream) => return Client::new(stream, read_timeout).map_err(ConnectError::Io),
                Err(e) => {
                    let missing = matches!(
                        e.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    );

                    if !missing {
                        return Err(ConnectError::Io(e));
                    }

                    if Instant::now() >= deadline {
                        return Err(ConnectError::NotRunning);
                    }

                    std::thread::sleep(CONNECT_INTERVAL);
                }
            }
        }
    }

    pub fn attach(mut self) -> Result<Attached> {
        let (response, fds) = self.exchange(Request::Attach)?;
        let Response::Attach(reply) = response else {
            unreachable!();
        };

        if fds.is_empty() {
            bail!("received empty namespace fd set");
        }

        if fds.len() != reply.namespaces.len() {
            bail!(
                "received {} namespace descriptors for {} namespaces",
                fds.len(),
                reply.namespaces.len()
            );
        }

        Ok(Attached {
            lease: self.stream,
            reply,
            fds,
        })
    }

    pub fn status(&mut self) -> Result<StatusReply> {
        let (response, _) = self.exchange(Request::Status)?;
        let Response::Status(reply) = response else {
            unreachable!();
        };

        Ok(reply)
    }

    pub fn stop(&mut self) -> Result<()> {
        let (response, _) = self.exchange(Request::Stop)?;
        let Response::Stop = response else {
            unreachable!();
        };

        Ok(())
    }

    /// Send one request and decode its reply. Error replies and replies of the
    /// wrong kind become errors here, so callers only see the answer they
    /// asked for.
    fn exchange(&mut self, request: Request) -> Result<(Response, Vec<OwnedFd>)> {
        self.stream
            .write_all(&request.encode())
            .context("sending a request")?;

        let mut parser = ResponseParser::new();
        let mut fds = Vec::new();

        let response = loop {
            // The parser reserves exactly what is missing and lets recvmsg
            // fill it in place: no intermediate buffer, no copy.
            let bytes = {
                let mut cmsg_buf = [0u8; 128];
                let mut iov = [IoSliceMut::new(parser.spare_mut())];

                let msg = recvmsg::<()>(
                    self.stream.as_raw_fd(),
                    &mut iov,
                    Some(&mut cmsg_buf),
                    MsgFlags::MSG_CMSG_CLOEXEC,
                )
                .context("reading a message")?;

                if msg.bytes == 0 {
                    bail!("the sandbox closed the connection");
                }

                for cmsg in msg.cmsgs()? {
                    if let ControlMessageOwned::ScmRights(received) = cmsg {
                        fds.extend(
                            received
                                .into_iter()
                                .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) }),
                        );
                    }
                }

                msg.bytes
            };

            if let Some(response) = parser.commit(bytes)? {
                break response;
            }
        };

        let response = match response {
            Response::Error(message) => bail!("server response error: {message}"),
            response => response,
        };

        if response.code() != request.code() {
            bail!(
                "server response action '{}' instead of '{}'",
                response.code(),
                request.code()
            );
        }

        Ok((response, fds))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::sys::socket::{ControlMessage, sendmsg};
    use std::collections::BTreeMap;
    use std::io::IoSlice;
    use std::os::fd::RawFd;

    fn write_response(stream: &UnixStream, response: &Response, fds: &[RawFd]) {
        let frame = response.encode().expect("encode");
        let mut offset = 0;

        while offset < frame.len() {
            let iov = [IoSlice::new(&frame[offset..])];
            let cmsg: Vec<ControlMessage> = if offset == 0 && !fds.is_empty() {
                vec![ControlMessage::ScmRights(fds)]
            } else {
                Vec::new()
            };

            let sent = sendmsg::<()>(
                stream.as_raw_fd(),
                &iov,
                &cmsg,
                MsgFlags::MSG_NOSIGNAL,
                None,
            )
            .expect("send");

            assert!(sent > 0, "the reply made no progress");
            offset += sent;
        }
    }

    #[test]
    fn an_error_reply_becomes_an_error() {
        let (client, peer) = UnixStream::pair().expect("socketpair");
        write_response(&peer, &Response::Error("sandbox is busy".to_string()), &[]);

        let mut client = Client::new(client, Duration::from_secs(1)).expect("client");
        let error = client.status().expect_err("error reply");
        assert!(format!("{error:#}").contains("sandbox is busy"));
    }

    #[test]
    fn an_unexpected_reply_is_an_error() {
        let (client, peer) = UnixStream::pair().expect("socketpair");
        write_response(&peer, &Response::Stop, &[]);

        let mut client = Client::new(client, Duration::from_secs(1)).expect("client");
        let error = client.status().expect_err("unexpected reply");
        let message = format!("{error:#}");
        assert!(message.contains("server response action '84' instead of '83'"));
    }

    #[test]
    fn attach_needs_one_descriptor_per_namespace() {
        let (client, peer) = UnixStream::pair().expect("socketpair");
        let file = std::fs::File::open("/dev/null").expect("open");
        let reply = AttachReply {
            env: BTreeMap::new(),
            namespaces: Vec::new(),
        };
        write_response(&peer, &Response::Attach(reply), &[file.as_raw_fd()]);

        let client = Client::new(client, Duration::from_secs(1)).expect("client");
        let error = client.attach().expect_err("descriptor mismatch");
        assert!(format!("{error:#}").contains("namespace descriptors"));
    }

    #[test]
    fn a_status_reply_survives_the_wire() {
        let reply = StatusReply {
            dir: "/home/user/Sandbox/demo".to_string(),
            pid: 4242,
            started: 1_700_000_000,
            sessions: vec![11, 22],
        };
        let (client, peer) = UnixStream::pair().expect("socketpair");
        write_response(&peer, &Response::Status(reply.clone()), &[]);

        let mut client = Client::new(client, Duration::from_secs(1)).expect("client");
        assert_eq!(client.status().expect("status"), reply);
    }
}
