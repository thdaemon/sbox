// SPDX-FileCopyrightText: 2026 Eric Tian <thxdaemon@gmail.com>
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The language shared by the client and the sandbox daemon.
//!
//! This module is pure: it turns messages into bytes and bytes into messages.
//! It never touches a socket, a timeout or a file descriptor. Moving those
//! bytes, and carrying descriptors with them, belongs to app/client and the
//! daemon peer.
//!
//! Every message is a frame:
//!
//!     "SBOX" | version (u16) | action (u16) | size (u32) | payload
//!     <- 4 ->     <- 2 ->         <- 2 ->       <- 4 ->    <- size ->
//!
//! The header is what makes the stream self-describing: a receiver knows how
//! many payload bytes belong to one message instead of hoping a single read
//! returns all of it, and a message larger than announced is an error instead
//! of a silent truncation. The action lets the daemon route a request before
//! it decodes anything.
//!
//! The payload is postcard, so the messages are derived from their types
//! instead of written out by hand.
//!
//! Encoding produces a complete frame. Decoding goes through RequestParser or
//! ResponseParser: they own the bytes seen so far, validate the header as soon
//! as it arrives, and report how many bytes are still missing. Neither encode
//! nor decode makes a caller deal with the header.

use anyhow::{Context, Result, bail};
use serde::Serialize;
use std::collections::BTreeMap;

/// Size of the frame header: magic, version, action and payload length.
const HEADER_LEN: usize = 12;
const MAGIC: [u8; 4] = *b"SBOX";
const VERSION: u16 = 1;

/// Nothing legitimate comes close to this, and it bounds what a confused peer
/// can make the other side allocate.
const MAX_PAYLOAD: usize = 256 * 1024;

const ATTACH: u16 = b'A' as u16;
const STATUS: u16 = b'S' as u16;
const STOP: u16 = b'T' as u16;
const ERROR: u16 = b'E' as u16;

/// What the sandbox does with one variable of the session environment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub(crate) enum EnvOp {
    Set(String),
    Unset,
}

/// The environment a session starts from: what the sandbox sets and what it
/// drops. Everything else is the environment of the terminal that attaches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub(crate) struct AttachReply {
    /// Keyed by variable name, so a variable has exactly one fate and cannot
    /// be both set and removed.
    pub env: BTreeMap<String, EnvOp>,
    /// The namespace descriptors sent with this reply, in join order.
    pub namespaces: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub(crate) struct StatusReply {
    pub dir: String,
    pub pid: i32,
    pub started: u64,
    pub sessions: Vec<u32>,
}

/// What a client asks the daemon to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Request {
    Attach,
    Status,
    Stop,
}

impl Request {
    /// The whole frame for this request. A request has no payload, so this is
    /// only its header today.
    pub(crate) fn encode(self) -> Vec<u8> {
        Header::new(self.code(), 0).encode().to_vec()
    }

    fn decode(header: &Header, payload: &[u8]) -> Result<Request> {
        debug_assert!(
            payload.is_empty(),
            "Currently, all kinds of requests have no payload"
        );

        match header.action {
            ATTACH => Ok(Request::Attach),
            STATUS => Ok(Request::Status),
            STOP => Ok(Request::Stop),
            _ => bail!("unknown request"),
        }
    }

    pub(crate) fn code(self) -> u16 {
        match self {
            Request::Attach => ATTACH,
            Request::Status => STATUS,
            Request::Stop => STOP,
        }
    }
}

/// What the daemon answers. An error is a reply like any other, so a client
/// that asked for the wrong thing still learns why.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Response {
    Attach(AttachReply),
    Status(StatusReply),
    Stop,
    Error(String),
}

impl Response {
    /// Encode one complete frame. Descriptors are not part of the bytes; the
    /// sender attaches them to the first write.
    pub(crate) fn encode(&self) -> Result<Vec<u8>> {
        let payload = match self {
            Response::Attach(reply) => postcard::to_allocvec(reply),
            Response::Status(reply) => postcard::to_allocvec(reply),
            Response::Stop => Ok(Vec::new()),
            Response::Error(message) => postcard::to_allocvec(message),
        }
        .context("encoding a message")?;

        if payload.len() > MAX_PAYLOAD {
            bail!(
                "message of {} bytes exceeds the {MAX_PAYLOAD} byte limit",
                payload.len()
            );
        }

        let mut frame = Vec::with_capacity(HEADER_LEN + payload.len());
        frame.extend_from_slice(&Header::new(self.code(), payload.len()).encode());
        frame.extend_from_slice(&payload);

        Ok(frame)
    }

    fn decode(header: &Header, payload: &[u8]) -> Result<Response> {
        let response: postcard::Result<Response> = match header.action {
            ATTACH => postcard::from_bytes(payload).map(Response::Attach),
            STATUS => postcard::from_bytes(payload).map(Response::Status),
            STOP => Ok(Response::Stop),
            ERROR => postcard::from_bytes(payload).map(Response::Error),
            action => bail!("the sandbox answered with unknown action {action}"),
        };

        response.context("decoding a message")
    }

    pub(crate) fn code(&self) -> u16 {
        match self {
            Response::Attach(_) => ATTACH,
            Response::Status(_) => STATUS,
            Response::Stop => STOP,
            Response::Error(_) => ERROR,
        }
    }
}

/// One header, once it is known to be complete.
struct Header {
    action: u16,
    size: usize,
}

impl Header {
    /// Validate a header at the start of a peer's bytes. None means fewer than
    /// HEADER_LEN bytes have arrived; the header may not be judged yet.
    fn parse(bytes: &[u8]) -> Result<Option<Header>> {
        if bytes.len() < HEADER_LEN {
            return Ok(None);
        }

        let mut header = [0u8; HEADER_LEN];
        header.copy_from_slice(&bytes[..HEADER_LEN]);

        Ok(Some(Header::decode(&header)?))
    }

    /// Validate a complete header.
    fn decode(header: &[u8; HEADER_LEN]) -> Result<Header> {
        if header[..4] != MAGIC {
            bail!("the sandbox speaks a protocol sbox does not understand");
        }

        let version = u16::from_be_bytes([header[4], header[5]]);
        if version != VERSION {
            bail!(
                "the sandbox speaks protocol version {}, sbox speaks {}",
                version,
                VERSION
            );
        }

        let action = u16::from_be_bytes([header[6], header[7]]);
        let size = u32::from_be_bytes([header[8], header[9], header[10], header[11]]) as usize;
        if size > MAX_PAYLOAD {
            bail!("the sandbox announced a {size} byte message, the limit is {MAX_PAYLOAD}");
        }

        Ok(Header { action, size })
    }

    /// Build a header for a frame that is about to be encoded.
    fn new(action: u16, size: usize) -> Header {
        Header { action, size }
    }

    /// Encode this header into the first bytes of a frame.
    fn encode(&self) -> [u8; HEADER_LEN] {
        let mut header = [0u8; HEADER_LEN];

        header[..4].copy_from_slice(&MAGIC);
        header[4..6].copy_from_slice(&VERSION.to_be_bytes());
        header[6..8].copy_from_slice(&self.action.to_be_bytes());
        header[8..].copy_from_slice(&(self.size as u32).to_be_bytes());

        header
    }
}

/// Bytes seen so far for one frame, and the header once it is complete.
///
/// This is the shared state machine behind both parsers. It reserves room for
/// exactly what is still missing, lets the reader fill that memory in place,
/// and validates the header as soon as 12 bytes have been committed.
struct FrameParser {
    buf: Vec<u8>,
    header: Option<Header>,
    /// Size of the slice handed out by the most recent spare_mut call.
    spare: usize,
}

impl FrameParser {
    fn new() -> FrameParser {
        FrameParser {
            buf: Vec::new(),
            header: None,
            spare: 0,
        }
    }

    /// Reserve room for the missing bytes and hand it to the caller.
    ///
    /// The returned memory is uninitialised. It may only be written; commit
    /// makes the written prefix readable.
    fn spare_mut(&mut self) -> &mut [u8] {
        let needed = self.needed();
        self.buf.reserve(needed);

        let len = self.buf.len();
        let spare = (self.buf.capacity() - len).min(needed);
        self.spare = spare;

        // SAFETY: the slice lies inside the allocation and starts after the
        // initialised prefix. The caller only writes it, and commit bounds the
        // part that can be read by how much the caller says it wrote.
        unsafe { std::slice::from_raw_parts_mut(self.buf.as_mut_ptr().add(len), spare) }
    }

    /// Accept len bytes the caller wrote into the last spare slice. A length
    /// larger than the reserved slice is an error; the caller must actually
    /// have written the prefix it commits.
    fn commit(&mut self, len: usize) -> Result<()> {
        if len > self.spare {
            bail!(
                "committed {len} bytes, but only {} were reserved",
                self.spare
            );
        }

        let end = self.buf.len() + len;
        // SAFETY: spare_mut reserved this much room, and the caller reports
        // that it wrote the prefix being committed.
        unsafe { self.buf.set_len(end) };
        self.spare = 0;

        self.read_header()
    }

    /// The header and payload of a complete frame, if one is ready.
    fn frame(&self) -> Option<(&Header, &[u8])> {
        let header = self.header.as_ref()?;
        let end = HEADER_LEN + header.size;
        if self.buf.len() < end {
            return None;
        }
        Some((header, &self.buf[HEADER_LEN..end]))
    }

    /// Drop the frame just decoded and parse the header of whatever follows.
    fn consume(&mut self) -> Result<()> {
        if let Some(header) = self.header.take() {
            self.buf.drain(..HEADER_LEN + header.size);
        }
        self.read_header()
    }

    /// How many bytes are still missing before a frame is ready.
    fn needed(&self) -> usize {
        match self.header.as_ref() {
            Some(header) => (HEADER_LEN + header.size).saturating_sub(self.buf.len()),
            None => HEADER_LEN.saturating_sub(self.buf.len()),
        }
    }

    fn read_header(&mut self) -> Result<()> {
        if self.header.is_none() && self.buf.len() >= HEADER_LEN {
            self.header = Header::parse(&self.buf)?;
        }
        Ok(())
    }
}

/// Incremental decoder for replies.
///
/// The client asks for spare_mut, reads straight into it, and commits what it
/// wrote. A malformed header fails as soon as it is complete, without waiting
/// for the payload.
pub(crate) struct ResponseParser {
    inner: FrameParser,
}

impl ResponseParser {
    pub fn new() -> ResponseParser {
        ResponseParser {
            inner: FrameParser::new(),
        }
    }

    /// Room for the bytes still missing. Write into it, then commit.
    ///
    /// The slice is uninitialised; only writing is allowed until commit.
    pub fn spare_mut(&mut self) -> &mut [u8] {
        self.inner.spare_mut()
    }

    /// Accept len bytes written into the last spare slice and decode a reply
    /// if the frame is now complete. A length larger than the reserved slice
    /// is an error; the caller must have filled the prefix it commits.
    pub fn commit(&mut self, len: usize) -> Result<Option<Response>> {
        self.inner.commit(len)?;

        let Some((header, payload)) = self.inner.frame() else {
            return Ok(None);
        };

        let response = Response::decode(header, payload)?;
        self.inner.consume()?;

        Ok(Some(response))
    }
}

impl Default for ResponseParser {
    fn default() -> Self {
        ResponseParser::new()
    }
}

/// Incremental decoder for requests.
///
/// Requests carry no payload, so one with a non-zero size is rejected as soon
/// as its header arrives, without waiting for the bytes it claims. The daemon
/// uses the same spare_mut/commit path as the client.
pub(crate) struct RequestParser {
    inner: FrameParser,
}

impl RequestParser {
    pub fn new() -> RequestParser {
        RequestParser {
            inner: FrameParser::new(),
        }
    }

    /// Room for the bytes still missing. Write into it, then commit.
    ///
    /// The slice is uninitialised; only writing is allowed until commit.
    pub fn spare_mut(&mut self) -> &mut [u8] {
        self.inner.spare_mut()
    }

    /// Accept len bytes written into the last spare slice and decode a request
    /// if the frame is now complete. A length larger than the reserved slice
    /// is an error; the caller must have filled the prefix it commits.
    pub fn commit(&mut self, len: usize) -> Result<Option<Request>> {
        self.inner.commit(len)?;

        let Some((header, payload)) = self.inner.frame() else {
            return Ok(None);
        };

        let request = Request::decode(header, payload)?;
        self.inner.consume()?;

        Ok(Some(request))
    }
}

impl Default for RequestParser {
    fn default() -> Self {
        RequestParser::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attach_reply() -> AttachReply {
        let mut env = BTreeMap::new();
        env.insert(
            "DBUS_SESSION_BUS_ADDRESS".to_string(),
            EnvOp::Set("unix:path=/run/user/1000/bus".to_string()),
        );
        env.insert("EMPTY".to_string(), EnvOp::Set(String::new()));
        env.insert("WITH_EQUALS".to_string(), EnvOp::Set("a=b".to_string()));
        env.insert("DISPLAY".to_string(), EnvOp::Unset);
        env.insert("VTE_VERSION".to_string(), EnvOp::Unset);
        AttachReply {
            env,
            namespaces: vec!["user".to_string(), "mnt".to_string()],
        }
    }

    fn status_reply() -> StatusReply {
        StatusReply {
            dir: "/home/user/Sandbox/demo".to_string(),
            pid: 4242,
            started: 1_700_000_000,
            sessions: vec![11, 22],
        }
    }

    fn feed_request(parser: &mut RequestParser, mut bytes: &[u8]) -> Result<Option<Request>> {
        let mut result = None;
        while !bytes.is_empty() {
            let spare = parser.spare_mut();
            let take = spare.len().min(bytes.len());
            spare[..take].copy_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            result = parser.commit(take)?;
            if result.is_some() {
                break;
            }
        }
        Ok(result)
    }

    fn feed_response(parser: &mut ResponseParser, mut bytes: &[u8]) -> Result<Option<Response>> {
        let mut result = None;
        while !bytes.is_empty() {
            let spare = parser.spare_mut();
            let take = spare.len().min(bytes.len());
            spare[..take].copy_from_slice(&bytes[..take]);
            bytes = &bytes[take..];
            result = parser.commit(take)?;
            if result.is_some() {
                break;
            }
        }
        Ok(result)
    }

    fn parse_response(bytes: &[u8]) -> Response {
        let mut parser = ResponseParser::new();
        feed_response(&mut parser, bytes)
            .expect("parse")
            .expect("complete frame")
    }

    #[test]
    fn requests_round_trip() {
        for request in [Request::Attach, Request::Status, Request::Stop] {
            let mut parser = RequestParser::new();
            let bytes = request.encode();
            assert_eq!(
                feed_request(&mut parser, &bytes).expect("parse"),
                Some(request)
            );
            assert_eq!(parser.spare_mut().len(), HEADER_LEN);
        }
    }

    #[test]
    fn an_incomplete_request_waits_for_more() {
        let bytes = Request::Status.encode();
        let mut parser = RequestParser::new();

        assert_eq!(parser.spare_mut().len(), HEADER_LEN);
        assert_eq!(feed_request(&mut parser, &bytes[..8]).expect("feed"), None);
        assert_eq!(parser.spare_mut().len(), HEADER_LEN - 8);
        assert_eq!(
            feed_request(&mut parser, &bytes[8..]).expect("feed"),
            Some(Request::Status)
        );
    }

    #[test]
    fn an_unknown_request_is_refused() {
        let bytes = Header::new(0x1234, 0).encode();
        let mut parser = RequestParser::new();
        assert!(feed_request(&mut parser, &bytes).is_err());
    }

    #[test]
    fn replies_round_trip() {
        let responses = [
            Response::Attach(attach_reply()),
            Response::Status(status_reply()),
            Response::Stop,
            Response::Error("sandbox is busy".to_string()),
        ];

        for response in responses {
            let bytes = response.encode().expect("encode");
            assert_eq!(parse_response(&bytes), response);
        }
    }

    #[test]
    fn an_incomplete_reply_waits_for_more() {
        let response = Response::Status(status_reply());
        let bytes = response.encode().expect("encode");
        let mut parser = ResponseParser::new();

        assert_eq!(feed_response(&mut parser, &bytes[..8]).expect("feed"), None);
        assert_eq!(
            feed_response(&mut parser, &bytes[8..HEADER_LEN]).expect("feed"),
            None
        );
        assert_eq!(parser.spare_mut().len(), bytes.len() - HEADER_LEN);
        assert_eq!(
            feed_response(&mut parser, &bytes[HEADER_LEN..]).expect("feed"),
            Some(response)
        );
    }

    #[test]
    fn a_parser_starts_a_new_frame_after_one_completes() {
        let mut parser = RequestParser::new();

        assert_eq!(
            feed_request(&mut parser, &Request::Status.encode()).expect("feed"),
            Some(Request::Status)
        );
        assert_eq!(
            feed_request(&mut parser, &Request::Stop.encode()).expect("feed"),
            Some(Request::Stop)
        );
    }

    #[test]
    fn truncated_payload_is_an_error() {
        let encoded = postcard::to_allocvec(&status_reply()).expect("encode");
        assert!(postcard::take_from_bytes::<StatusReply>(&encoded[..encoded.len() / 2]).is_err());
    }

    #[test]
    fn a_header_is_validated_before_anything_else() {
        let mut header = Header::new(STATUS, 0).encode();
        header[0] = b'X';
        assert!(Header::decode(&header).is_err());

        header = Header::new(STATUS, 0).encode();
        header[4] = VERSION.to_be_bytes()[0];
        header[5] = VERSION.to_be_bytes()[1] + 1;
        assert!(Header::decode(&header).is_err());

        header = Header::new(STATUS, MAX_PAYLOAD + 1).encode();
        assert!(Header::decode(&header).is_err());
    }
}
