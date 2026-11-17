//! Thin wrappers over the Linux interfaces sbox needs, with no policy of their
//! own: mounts in fs, event loops in epoll, namespaces, identity and process
//! control in sys.

pub(crate) mod epoll;
pub(crate) mod fs;
pub(crate) mod sys;

#[cfg(test)]
pub(crate) mod testing;
