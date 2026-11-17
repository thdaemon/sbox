//! Build the sandbox root and apply its configured mounts.
//!
//! Everything the host session provides is mounted and described in the same
//! block: the descriptor a session joins and the variable that points a
//! program at it are decided together, so they cannot disagree.

use super::{HostEntryKind, Root};
use crate::config::{BindMode, Config, PkiMode, XdgDirs};
use crate::sandbox::dbus_proxy::DbusProxySockets;
use crate::sandbox::environment::{HostEnv, SandboxEnv};
use anyhow::Result;
use std::path::Path;

/// Builds the mount tree of one sandbox, section by section.
///
/// Each method mounts one group of host paths and leaves the root ready for
/// the next. The order matters: a bind target must exist before it is mounted
/// on, the sandbox home has to be in place before the GUI workarounds go under
/// it, and pivot_root() comes last.
pub(in crate::sandbox) struct Builder<'a> {
    config: &'a Config,
    dbus_proxies: &'a DbusProxySockets,
    dirs: &'a XdgDirs,
    host: &'a HostEnv,
    root: Root,
}

impl<'a> Builder<'a> {
    pub fn build(
        config: &'a Config,
        host: &'a HostEnv,
        dbus_proxies: &'a DbusProxySockets,
        env: &mut SandboxEnv,
    ) -> Result<()> {
        let builder = Builder {
            config,
            dbus_proxies,
            dirs: config.dirs(),
            host,
            root: Root::create()?,
        };

        builder.private_root()?;
        builder.base_system()?;

        builder.home(env)?;

        builder.displays(env)?;
        builder.dbus(env)?;
        builder.gui()?;
        builder.user_shares()?;

        builder.finish()
    }

    /// An empty root with the pseudo filesystems every program expects.
    fn private_root(&self) -> Result<()> {
        self.root.procfs("proc")?;
        self.root.setup_dev()?;
        self.root.dir("/sys")?;
        // Shared temporary directories are world-writable and sticky, as they
        // are on the host.
        self.root.dir_mode("/tmp", 0o1777)?;
        self.root.dir("/var")?;
        self.root.dir_mode("/var/tmp", 0o1777)?;
        self.root.dir("/run")?;
        self.root.symlink("/run", "/var/run")?;
        self.root.dir_mode(self.dirs.runtime_dir(), 0o700)
    }

    fn base_system(&self) -> Result<()> {
        self.root.bind_same("/usr", BindMode::Ro)?;
        self.root.bind_same_try("/opt", BindMode::Ro)?;
        self.root.bind_same("/etc", BindMode::Ro)?;
        self.root.bind_same_try("/var/cache", BindMode::Ro)?;
        self.root.bind_same("/sys", BindMode::Ro)?;

        for dir in ["/lib", "/lib64", "/bin", "/sbin"] {
            self.usr_link(dir)?;
        }

        if self.config.sandbox.shares.resolv_conf {
            // /etc/resolv.conf is usually a symlink to a file that does not
            // exist in the sandbox. The bind attaches to the link's descriptor
            // (the new mount API allows it), so the mount shadows the link and
            // its target never has to exist; no host realpath is involved.
            let _ = self.root.bind_same_try("/etc/resolv.conf", BindMode::Ro);
        }
        Ok(())
    }

    /// /lib and friends are symlinks into /usr on a merged-/usr system; mirror
    /// that instead of binding the symlink on top of itself.
    fn usr_link(&self, dir: &str) -> Result<()> {
        let link = Path::new(dir);

        match self.root.host_entry(link)? {
            HostEntryKind::Missing => Ok(()),
            HostEntryKind::Other => self.root.bind_same(link, BindMode::Ro),
            HostEntryKind::SymLink(target) => {
                // A relative target is relative to the filesystem root.
                let resolved = if target.is_absolute() {
                    target
                } else {
                    Path::new("/").join(target)
                };

                if resolved.starts_with("/usr") || resolved.starts_with("/opt") {
                    self.root.symlink(&resolved, link)
                } else {
                    self.root.bind(&resolved, link, BindMode::Ro)
                }
            }
        }
    }

    /// The display and audio sockets, and the variables that name them.
    fn displays(&self, env: &mut SandboxEnv) -> Result<()> {
        if self.config.sandbox.shares.x11 {
            self.x11_sockets(env)?;
            self.x11_credentials(env)?;
        } else {
            env.drop("DISPLAY");
            env.drop("XAUTHORITY");
        }

        if self.config.sandbox.shares.wayland {
            self.wayland(env)?;
        } else {
            env.drop("WAYLAND_DISPLAY");
        }

        if self.config.sandbox.shares.audio {
            self.audio(env)?;
        } else {
            env.drop("PIPEWIRE_REMOTE");
            env.drop("PIPEWIRE_RUNTIME_DIR");
            env.drop("PULSE_SERVER");
        }

        if self.config.sandbox.shares.dri {
            self.root.bind_same_try("/dev/dri", BindMode::Dev)?;
        }

        Ok(())
    }

    /// The X display number of a local unix socket, when DISPLAY names one.
    ///
    /// `:0` and `unix/:0` are the shorthand forms and `localhost:0` names the same
    /// server; anything with a host name in it is a network display, which has no
    /// socket to mount.
    fn x11_unix_socket_number(display: &str) -> Option<String> {
        let rest = if let Some(rest) = display.strip_prefix("localhost:") {
            rest
        } else if let Some(rest) = display.strip_prefix("unix/") {
            rest.strip_prefix(':')?
        } else {
            display.strip_prefix(':')?
        };

        let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
        if num.is_empty() { None } else { Some(num) }
    }

    /// The sockets a local display connects through. A remote one has none:
    /// only the credentials below are needed for it.
    fn x11_sockets(&self, env: &mut SandboxEnv) -> Result<()> {
        if let Some(display) = self.host.display.as_deref() {
            if let Some(num) = Self::x11_unix_socket_number(display) {
                self.root.bind_same_try("/tmp/.ICE-unix", BindMode::Dev)?;
                for name in [
                    format!("/tmp/.X11-unix/X{num}"),
                    format!("/tmp/.X{num}-lock"),
                ] {
                    self.root.bind_same_try(&name, BindMode::Dev)?;
                }
            };

            env.set("DISPLAY", display);
        }

        Ok(())
    }

    /// The cookie file authenticates the connection whatever the transport,
    /// so a TCP display needs it as much as a unix socket does. Mounted after
    /// the home, since it is usually inside it.
    fn x11_credentials(&self, env: &mut SandboxEnv) -> Result<()> {
        if let Some(auth) = self.host.xauthority.as_deref() {
            self.root.bind_same_try(auth, BindMode::Ro)?;
            env.set("XAUTHORITY", auth);
        }

        self.root
            .bind_same_try(self.dirs.home().join(".ICEauthority"), BindMode::Ro)?;

        Ok(())
    }

    fn wayland(&self, env: &mut SandboxEnv) -> Result<()> {
        let display = self.host.wayland_display.as_deref().unwrap_or("wayland-0");

        for name in [display.to_string(), format!("{display}.lock")] {
            self.root
                .bind_same_try(self.dirs.runtime_dir().join(&name), BindMode::Dev)?;
        }

        if let Some(value) = self.host.wayland_display.as_deref() {
            env.set("WAYLAND_DISPLAY", value);
        }

        Ok(())
    }

    fn audio(&self, env: &mut SandboxEnv) -> Result<()> {
        let remote = self.host.pipewire_remote.as_deref().unwrap_or("pipewire-0");
        let dir = self
            .host
            .pipewire_runtime_dir
            .as_deref()
            .unwrap_or_else(|| self.dirs.runtime_dir());

        for name in [remote.to_string(), format!("{remote}.lock")] {
            self.root.bind_same_try(dir.join(&name), BindMode::Dev)?;
        }

        // TODO: use PULSE_SERVER env-var
        self.root
            .bind_same_try(self.dirs.runtime_dir().join("pulse/native"), BindMode::Dev)?;

        if let Some(remote) = self.host.pipewire_remote.as_deref() {
            env.set("PIPEWIRE_REMOTE", remote);
        }
        if let Some(dir) = &self.host.pipewire_runtime_dir {
            env.set("PIPEWIRE_RUNTIME_DIR", dir.display().to_string());
        }

        Ok(())
    }

    fn dbus(&self, env: &mut SandboxEnv) -> Result<()> {
        if self.config.sandbox.dbus.raw {
            if let Some(address) = self.host.dbus_system.as_deref() {
                env.set("DBUS_SYSTEM_BUS_ADDRESS", address);

                if let Some(path) = address.strip_prefix("unix:path=") {
                    self.root
                        .bind_try(path, "/run/dbus/system_bus_socket", BindMode::Dev)?;
                }
            } else {
                self.root
                    .bind_same_try("/run/dbus/system_bus_socket", BindMode::Dev)?;
            }

            if let Some(address) = self.host.dbus_session.as_deref() {
                env.set("DBUS_SESSION_BUS_ADDRESS", address);

                if let Some(path) = address.strip_prefix("unix:path=") {
                    self.root.bind_same_try(path, BindMode::Dev)?;
                }
            }
        } else {
            if let Some(sock) = &self.dbus_proxies.system {
                self.root
                    .bind_try(sock, "/run/dbus/system_bus_socket", BindMode::Dev)?;
            }

            if let Some(sock) = &self.dbus_proxies.session {
                let dest = self.dirs.runtime_dir().join("bus");
                self.root.bind_try(sock, &dest, BindMode::Dev)?;

                env.set(
                    "DBUS_SESSION_BUS_ADDRESS",
                    format!("unix:path={}", dest.display()),
                );
            } else {
                env.drop("DBUS_SESSION_BUS_ADDRESS");
            }
        }

        Ok(())
    }

    fn home(&self, env: &mut SandboxEnv) -> Result<()> {
        self.root
            .bind(self.config.sandbox_dir(), self.dirs.home(), BindMode::Rw)?;

        env.set("HOME", self.dirs.home().display().to_string());

        let pki = self.dirs.home().join(".pki");
        match self.config.sandbox.shares.pki {
            PkiMode::Ro => {
                self.root.bind_same_try(&pki, BindMode::Ro)?;
            }
            PkiMode::Rw => {
                self.root.bind_same_try(&pki, BindMode::Rw)?;
            }
            PkiMode::Isolate => {}
        }

        Ok(())
    }

    /// Config and data directories. They live under $HOME, so they are
    /// mounted after it, at the paths a program uses once the XDG directories
    /// are dropped.
    fn gui(&self) -> Result<()> {
        let config_home = self.dirs.home().join(".config");
        let data_home = self.dirs.home().join(".local/share");

        if self.config.sandbox.gui.gtk_config {
            for version in ["gtk-3.0", "gtk-4.0"] {
                self.root.bind_try(
                    self.dirs.config_home().join(version),
                    config_home.join(version),
                    BindMode::Ro,
                )?;
            }
        }

        if self.config.sandbox.gui.icons {
            self.root
                .bind_same_try(self.dirs.home().join(".icons"), BindMode::Ro)?;
            self.root.bind_try(
                self.dirs.data_home().join("icons"),
                data_home.join("icons"),
                BindMode::Ro,
            )?;
        }

        if self.config.sandbox.gui.fontconfig {
            self.root.bind_try(
                self.dirs.config_home().join("fontconfig"),
                config_home.join("fontconfig"),
                BindMode::Ro,
            )?;
        }

        if self.config.sandbox.gui.fonts {
            self.root.bind_try(
                self.dirs.data_home().join("fonts"),
                data_home.join("fonts"),
                BindMode::Ro,
            )?;
        }

        Ok(())
    }

    fn user_shares(&self) -> Result<()> {
        for bind in &self.config.sandbox.binds {
            let source = self.config.dirs().expand(&bind.source);
            let dest = bind
                .dest
                .as_deref()
                .map(|dest| self.config.dirs().expand(dest))
                .unwrap_or_else(|| source.clone());

            self.root.bind(&source, &dest, bind.mode)?;
        }
        Ok(())
    }

    fn finish(self) -> Result<()> {
        self.root.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::Builder;

    #[test]
    fn the_socket_number_comes_from_a_local_display() {
        for (display, expected) in [
            (":0", Some("0")),
            (":0.0", Some("0")),
            (":12", Some("12")),
            ("localhost:1", Some("1")),
            ("unix/:2", Some("2")),
            ("host:10.0", None),
            ("1.1.1.1:10.0", None),
            ("", None),
            (":", None),
        ] {
            assert_eq!(
                Builder::x11_unix_socket_number(display).as_deref(),
                expected,
                "display {display:?}"
            );
        }
    }
}
