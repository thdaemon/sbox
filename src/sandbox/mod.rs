// SPDX-FileCopyrightText: 2026 Eric Tian <thxdaemon@gmail.com>
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The sandbox side: bringing one into existence, building the mount tree,
//! entering its namespaces and preparing a session.

mod dbus_proxy;
mod environment;
mod mount;
mod session;
mod start;

pub(crate) use session::Session;
pub(crate) use start::Launcher;
