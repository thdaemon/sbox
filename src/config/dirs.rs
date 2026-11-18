// SPDX-FileCopyrightText: 2026 Eric Tian <thxdaemon@gmail.com>
// SPDX-License-Identifier: AGPL-3.0-or-later

//! The XDG directories sbox works with.

use crate::name::Name;
use std::path::{Path, PathBuf};

/// The entry names inside a sandbox's runtime directory. Keeping them here
/// means setup, the daemon and the client all agree on one layout.
pub(crate) const CONTROL_SOCKET: &str = "sock";
const LOCK: &str = "lock";
const DBUS_PROXY_SESSION_SOCKET: &str = "dbus-session.sock";
const DBUS_PROXY_SYSTEM_SOCKET: &str = "dbus-system.sock";

/// The directories a sandbox touches, resolved once so that every part of a
/// run agrees on them and a test can supply its own.
#[derive(Debug, Clone)]
pub(crate) struct XdgDirs {
    home: PathBuf,
    config: PathBuf,
    data: PathBuf,
    state: PathBuf,
    runtime: PathBuf,
}

impl XdgDirs {
    /// Resolve the directories from the environment the caller handed sbox.
    pub fn from_env() -> XdgDirs {
        fn xdg(variable: &str, fallback: PathBuf) -> PathBuf {
            match std::env::var_os(variable) {
                Some(value) if !value.is_empty() => PathBuf::from(value),
                _ => fallback,
            }
        }

        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));
        XdgDirs {
            config: xdg("XDG_CONFIG_HOME", home.join(".config")),
            data: xdg("XDG_DATA_HOME", home.join(".local/share")),
            state: xdg("XDG_STATE_HOME", home.join(".local/state")),
            runtime: xdg(
                "XDG_RUNTIME_DIR",
                PathBuf::from(format!("/run/user/{}", nix::unistd::Uid::current())),
            ),
            home,
        }
    }

    /// The same layout around an explicit home, for tests and for callers that
    /// already know where they are.
    #[cfg(test)]
    pub fn under(home: impl Into<PathBuf>) -> XdgDirs {
        let home = home.into();
        XdgDirs {
            config: home.join(".config"),
            data: home.join(".local/share"),
            state: home.join(".local/state"),
            runtime: home.join("run"),
            home,
        }
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn config_home(&self) -> &Path {
        &self.config
    }

    pub fn data_home(&self) -> &Path {
        &self.data
    }

    pub fn runtime_dir(&self) -> &Path {
        &self.runtime
    }

    /// Expand a leading ~/ the way the configuration file allows.
    pub fn expand(&self, path: &str) -> PathBuf {
        match path.strip_prefix("~/") {
            Some(rest) => self.home.join(rest),
            None if path == "~" => self.home.clone(),
            None => PathBuf::from(path),
        }
    }

    /// The program configuration: where the sandboxes live.
    pub fn main_config_file(&self) -> PathBuf {
        self.config.join("sbox/config.toml")
    }

    /// The global sandbox layer: defaults for every sandbox.
    pub fn global_config_file(&self) -> PathBuf {
        self.config.join("sbox/global.toml")
    }

    /// The per-sandbox layer.
    pub fn sandbox_config_file(&self, name: &str) -> PathBuf {
        self.config
            .join("sbox/sandbox.d")
            .join(format!("{name}.toml"))
    }

    pub fn state_dir(&self, name: &str) -> PathBuf {
        self.state.join("sbox").join(name)
    }

    /// Where the per-sandbox runtime directories live: in the runtime
    /// directory, so they are never reachable from inside a sandbox.
    pub fn socket_root(&self) -> PathBuf {
        self.runtime.join("sbox")
    }

    /// The runtime directory of one sandbox: its control socket, lock and dbus
    /// proxy sockets.
    pub fn socket_dir(&self, name: &Name) -> PathBuf {
        self.socket_root().join(name.as_str())
    }

    pub fn control_socket(&self, name: &Name) -> PathBuf {
        self.socket_dir(name).join(CONTROL_SOCKET)
    }

    /// The lock file that marks this sandbox name as taken.
    pub fn lock_file(&self, name: &Name) -> PathBuf {
        self.socket_dir(name).join(LOCK)
    }

    pub fn dbus_proxy_session_socket(&self, name: &Name) -> PathBuf {
        self.socket_dir(name).join(DBUS_PROXY_SESSION_SOCKET)
    }

    pub fn dbus_proxy_system_socket(&self, name: &Name) -> PathBuf {
        self.socket_dir(name).join(DBUS_PROXY_SYSTEM_SOCKET)
    }
}

#[cfg(test)]
mod tests {
    use super::XdgDirs;
    use crate::name::Name;
    use std::path::Path;

    fn name(text: &str) -> Name {
        Name::parse(text).expect("valid name")
    }

    #[test]
    fn paths_hang_off_the_directories() {
        let dirs = XdgDirs::under("/home/user");
        assert_eq!(
            dirs.main_config_file(),
            Path::new("/home/user/.config/sbox/config.toml")
        );
        assert_eq!(
            dirs.global_config_file(),
            Path::new("/home/user/.config/sbox/global.toml")
        );
        assert_eq!(
            dirs.sandbox_config_file("work"),
            Path::new("/home/user/.config/sbox/sandbox.d/work.toml")
        );
        assert_eq!(
            dirs.state_dir("work"),
            Path::new("/home/user/.local/state/sbox/work")
        );
    }

    #[test]
    fn sockets_do_not_depend_on_the_runtime_dir_existing() {
        let dirs = XdgDirs::under("/home/user");
        assert_eq!(dirs.socket_root(), Path::new("/home/user/run/sbox"));
        assert_eq!(
            dirs.socket_dir(&name("work")),
            Path::new("/home/user/run/sbox/work")
        );
    }

    #[test]
    fn runtime_files_live_below_the_sandbox_directory() {
        let dirs = XdgDirs::under("/home/user");
        let work = name("work");
        assert_eq!(
            dirs.control_socket(&work),
            Path::new("/home/user/run/sbox/work/sock")
        );
        assert_eq!(
            dirs.lock_file(&work),
            Path::new("/home/user/run/sbox/work/lock")
        );
        assert_eq!(
            dirs.dbus_proxy_session_socket(&work),
            Path::new("/home/user/run/sbox/work/dbus-session.sock")
        );
        assert_eq!(
            dirs.dbus_proxy_system_socket(&work),
            Path::new("/home/user/run/sbox/work/dbus-system.sock")
        );
    }

    #[test]
    fn expand_only_touches_a_leading_tilde() {
        let dirs = XdgDirs::under("/home/user");
        assert_eq!(dirs.expand("~/Sandbox"), Path::new("/home/user/Sandbox"));
        assert_eq!(dirs.expand("~"), Path::new("/home/user"));
        assert_eq!(dirs.expand("/tmp/x"), Path::new("/tmp/x"));
        assert_eq!(dirs.expand("relative"), Path::new("relative"));
    }
}
