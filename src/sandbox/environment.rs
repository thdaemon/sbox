// SPDX-FileCopyrightText: 2026 Eric Tian <thxdaemon@gmail.com>
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The host session a sandbox mirrors, and the environment a session starts
//! with.
//!
//! The host's display, audio and dbus addresses are snapshotted once, and the
//! mount tree records the variable each shared path is reached through. A
//! session then gets only what the sandbox dictates; everything else comes from
//! the terminal that attaches (the reply is applied in Session::enter).

use crate::config::{Config, EnvValue};
use crate::protocol::EnvOp;
use std::collections::BTreeMap;
use std::path::PathBuf;

/// Display, audio and dbus addresses, snapshotted once at startup so that the
/// mount tree and the session environment cannot disagree about them.
#[derive(Debug, Clone, Default)]
pub(super) struct HostEnv {
    /// The X display the sandbox was built around: it decided which socket was
    /// mounted, so a session cannot choose another one.
    pub display: Option<String>,
    pub wayland_display: Option<String>,
    pub xauthority: Option<String>,
    pub pipewire_remote: Option<String>,
    pub pipewire_runtime_dir: Option<PathBuf>,
    pub dbus_session: Option<String>,
    pub dbus_system: Option<String>,
}

impl HostEnv {
    pub fn snapshot() -> HostEnv {
        HostEnv::from_lookup(|key| std::env::var(key).ok())
    }

    /// An empty value means the variable is not set: WAYLAND_DISPLAY=""
    /// is what an unset variable expands to in a shell, and taking it
    /// literally would make join("") name the runtime directory itself.
    fn from_lookup(lookup: impl Fn(&str) -> Option<String>) -> HostEnv {
        let var = |key: &str| lookup(key).filter(|value| !value.is_empty());
        HostEnv {
            display: var("DISPLAY"),
            wayland_display: var("WAYLAND_DISPLAY"),
            xauthority: var("XAUTHORITY"),
            pipewire_remote: var("PIPEWIRE_REMOTE"),
            pipewire_runtime_dir: var("PIPEWIRE_RUNTIME_DIR").map(PathBuf::from),
            dbus_session: var("DBUS_SESSION_BUS_ADDRESS"),
            dbus_system: var("DBUS_SYSTEM_BUS_ADDRESS"),
        }
    }

    /// The system bus the host offers, if it can be reached. A non-path
    /// address is left for the proxy to connect to.
    pub fn dbus_system_bus(&self) -> Option<String> {
        let address = self
            .dbus_system
            .as_deref()
            .unwrap_or("unix:path=/run/dbus/system_bus_socket");

        let reachable = match address.strip_prefix("unix:path=") {
            Some(path) => std::os::unix::net::UnixStream::connect(path).is_ok(),
            None => true,
        };

        reachable.then(|| address.to_string())
    }
}

/// The environment a session starts with, built in one place: the baseline, then
/// the variables the mount tree records while it shares host paths, then the
/// user's configuration. Later entries win, and the configuration is last.
#[derive(Debug, Default)]
pub(super) struct SandboxEnv {
    env: BTreeMap<String, EnvOp>,
}

impl SandboxEnv {
    /// The baseline every session starts with: the variables that name host
    /// paths the sandbox does not have.
    pub fn new() -> SandboxEnv {
        let mut env = SandboxEnv::default();

        env.drop("VTE_VERSION");

        env.drop("XDG_DATA_HOME");
        env.drop("XDG_CONFIG_HOME");
        env.drop("XDG_STATE_HOME");
        env.drop("XDG_CACHE_HOME");

        // The sandbox does not inherit the host system bus address; it
        // normally exposes the bus at its default path instead.
        env.drop("DBUS_SYSTEM_BUS_ADDRESS");

        env
    }

    /// A variable a shared host path is reached through.
    pub fn set(&mut self, key: &str, value: impl Into<String>) {
        self.env.insert(key.to_string(), EnvOp::Set(value.into()));
    }

    /// A variable to remove: the sandbox does not share what it names.
    pub fn drop(&mut self, key: &str) {
        self.env.insert(key.to_string(), EnvOp::Unset);
    }

    /// The user's configuration, applied last: it can set a variable the
    /// sandbox dropped, or drop one it set.
    pub fn configure(&mut self, config: &Config) {
        if config.sandbox.gui.ibus {
            self.set("IBUS_USE_PORTAL", "1");
        }

        for (key, entry) in &config.sandbox.env {
            match entry {
                EnvValue::Value(value) => self.set(key, value.clone()),
                EnvValue::Remove => self.drop(key),
            }
        }
    }

    /// Consume the environment into the form the protocol carries.
    pub fn into_env(self) -> BTreeMap<String, EnvOp> {
        self.env
    }
}

#[cfg(test)]
mod tests {
    use super::{HostEnv, SandboxEnv};
    use crate::config::{Config, EnvValue};
    use crate::protocol::EnvOp;

    #[test]
    fn an_empty_host_variable_is_no_value() {
        let host = HostEnv::from_lookup(|_| Some(String::new()));

        // An empty name would join to the runtime directory itself, which
        // would bind the whole directory into the sandbox; the mount tree
        // falls back to "wayland-0" and "pipewire-0" when these are None.
        assert_eq!(host.wayland_display, None);
        assert_eq!(host.pipewire_remote, None);
        assert_eq!(host.pipewire_runtime_dir, None);
        assert_eq!(host.dbus_system, None);
    }

    #[test]
    fn a_non_path_system_bus_is_kept() {
        let host = HostEnv::from_lookup(|key| {
            (key == "DBUS_SYSTEM_BUS_ADDRESS").then(|| "unix:abstract=example".to_string())
        });

        assert_eq!(
            host.dbus_system_bus().as_deref(),
            Some("unix:abstract=example")
        );
    }

    #[test]
    fn a_missing_system_bus_path_is_not_offered() {
        let host = HostEnv::from_lookup(|key| {
            (key == "DBUS_SYSTEM_BUS_ADDRESS").then(|| "unix:path=/nonexistent".to_string())
        });

        assert_eq!(host.dbus_system_bus(), None);
    }

    #[test]
    fn the_baseline_drops_the_host_paths() {
        let env = SandboxEnv::new().into_env();
        for key in [
            "VTE_VERSION",
            "XDG_DATA_HOME",
            "XDG_CONFIG_HOME",
            "XDG_STATE_HOME",
            "XDG_CACHE_HOME",
            "DBUS_SYSTEM_BUS_ADDRESS",
        ] {
            assert_eq!(env.get(key), Some(&EnvOp::Unset), "{key}");
        }
    }

    #[test]
    fn the_configuration_can_put_a_baseline_variable_back() {
        let mut config = Config::under("/nonexistent-sbox-home");
        config.sandbox.env.insert(
            "XDG_DATA_HOME".into(),
            EnvValue::Value("/custom/data".into()),
        );
        let mut env = SandboxEnv::new();
        env.configure(&config);

        let reply = env.into_env();
        assert_eq!(
            reply.get("XDG_DATA_HOME"),
            Some(&EnvOp::Set("/custom/data".into()))
        );
        assert_eq!(reply.get("XDG_CONFIG_HOME"), Some(&EnvOp::Unset));
    }

    #[test]
    fn the_configuration_is_the_last_word() {
        let mut config = Config::under("/nonexistent-sbox-home");
        config
            .sandbox
            .env
            .insert("DISPLAY".into(), EnvValue::Value(":0".into()));
        let mut env = SandboxEnv::new();
        env.drop("DISPLAY");
        env.configure(&config);

        assert_eq!(
            env.into_env().get("DISPLAY"),
            Some(&EnvOp::Set(":0".into()))
        );
    }

    #[test]
    fn a_configured_entry_can_remove_what_the_mount_set() {
        let mut config = Config::under("/nonexistent-sbox-home");
        config
            .sandbox
            .env
            .insert("WAYLAND_DISPLAY".into(), EnvValue::Remove);
        let mut env = SandboxEnv::new();
        env.set("WAYLAND_DISPLAY", "wayland-1");
        env.configure(&config);

        assert_eq!(env.into_env().get("WAYLAND_DISPLAY"), Some(&EnvOp::Unset));
    }
}
