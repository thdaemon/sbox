// SPDX-FileCopyrightText: 2026 Eric Tian <thxdaemon@gmail.com>
// SPDX-License-Identifier: AGPL-3.0-or-later

//! What an epoll event in the daemon is about.
//!
//! The sandbox has one descriptor per session, so the watched set can grow;
//! epoll reports only the descriptors that are ready, instead of rescanning
//! the whole set on every call the way poll(2) would. Every registration
//! carries a tag: the kind of source in the high half of the token epoll
//! stores, the descriptor in the low half. A tag only has to identify the
//! registration while its fd is open, and a live fd is unique, so nothing has
//! to be counted.

use crate::linux::epoll::Source;
use crate::linux::sys;
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixListener;

/// What an epoll event is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub(super) enum TagKind {
    Listener = 0,
    Signals = 1,
    Peer = 2,
}

/// The tag epoll stores and hands back: the kind in the high half, the
/// descriptor in the low half.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct EpollTag {
    kind: TagKind,
    fd: u32,
}

impl EpollTag {
    /// The token a registration of this kind carries: the kind in the high
    /// half, the descriptor in the low half.
    pub fn token(kind: TagKind, fd: RawFd) -> u64 {
        ((kind as u64) << 32) | fd as u32 as u64
    }

    /// Read back what a registration stored. A kind this daemon never
    /// registers means the token did not come from it.
    pub fn decode(raw: u64) -> Option<EpollTag> {
        let kind = match (raw >> 32) as u32 {
            0 => TagKind::Listener,
            1 => TagKind::Signals,
            2 => TagKind::Peer,
            _ => return None,
        };
        Some(EpollTag {
            kind,
            fd: raw as u32,
        })
    }

    pub fn kind(self) -> TagKind {
        self.kind
    }

    /// The descriptor the tag was registered under.
    pub fn fd(self) -> RawFd {
        self.fd as RawFd
    }
}

/// The daemon listens on exactly one socket, and it is the listener.
impl Source for UnixListener {
    fn fd(&self) -> RawFd {
        self.as_raw_fd()
    }

    fn token(&self) -> u64 {
        EpollTag::token(TagKind::Listener, self.as_raw_fd())
    }
}

/// A signal descriptor is a signal descriptor to this daemon, and to nothing
/// else, so this is the only place that says so.
impl Source for sys::SignalFd {
    fn fd(&self) -> RawFd {
        self.as_raw_fd()
    }

    fn token(&self) -> u64 {
        EpollTag::token(TagKind::Signals, self.as_raw_fd())
    }
}

#[cfg(test)]
mod tests {
    use super::{EpollTag, TagKind};

    #[test]
    fn tags_survive_the_round_trip_through_epoll() {
        for (kind, fd) in [
            (TagKind::Listener, 3),
            (TagKind::Signals, 4),
            (TagKind::Peer, 7),
            (TagKind::Peer, i32::MAX),
        ] {
            let tag = EpollTag::decode(EpollTag::token(kind, fd)).expect("decoded");
            assert_eq!(tag.kind(), kind);
            assert_eq!(tag.fd(), fd);
        }
        assert_eq!(
            EpollTag::token(TagKind::Peer, 7),
            ((TagKind::Peer as u64) << 32) | 7
        );
        assert_eq!(EpollTag::decode(u64::MAX), None);
    }
}
