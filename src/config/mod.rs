// SPDX-FileCopyrightText: 2026 Eric Tian <thxdaemon@gmail.com>
// SPDX-License-Identifier: AGPL-3.0-or-later

//! Layered TOML configuration, all of it under $XDG_CONFIG_HOME/sbox.
//!
//! Two kinds of file: config.toml is the program's own configuration, today
//! only where the sandboxes live (sandbox_root). global.toml and
//! sandbox.d/<name>.toml describe the sandboxes themselves.
//!
//! A BaseConfig is built from the XDG directories and config.toml and names no
//! sandbox. BaseConfig::load_sandbox merges the sandbox files as TOML values,
//! deserializes the result once, and consumes the base: the Config it returns
//! carries the name and the settings, so a sandbox configuration cannot exist
//! without one. The caller then applies the command line. Defaults that depend
//! on the invocation -- the derived hostname, the sandbox directory, the
//! built-in dbus rules -- belong to the layer that uses them.

mod dirs;
mod load;
mod model;

pub(crate) use dirs::{CONTROL_SOCKET, SandboxPaths, XdgDirs};
pub(crate) use model::{Bind, BindMode, EnvValue, Hostname, PkiMode};

use crate::config::model::MainConfig;
use crate::name::Name;
use anyhow::{Context, Result, bail};
use load::ConfigLoader;
use model::SandboxConfig;
use std::path::PathBuf;

pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Lists that append across sandbox layers; every other value is replaced by
/// the later layer.
const SANDBOX_CONFIG_APPEND: &[&str] = &["bind", "dbus.session", "dbus.system"];

/// The program's own configuration: the directories sbox works with and
/// config.toml. It names no sandbox and holds no sandbox settings.
#[derive(Debug, Clone)]
pub(crate) struct BaseConfig {
    pub dirs: XdgDirs,
    main: MainConfig,
    /// Whether the invocation asked for the resolved settings to be printed.
    pub verbose: bool,
}

impl BaseConfig {
    /// Resolve the XDG directories and read config.toml, the program
    /// configuration.
    pub fn load() -> Result<BaseConfig> {
        BaseConfig::at(XdgDirs::from_env())
    }

    fn at(dirs: XdgDirs) -> Result<BaseConfig> {
        let path = dirs.main_config_file();
        let main: MainConfig = ConfigLoader::new(&[]).layer(&path)?.finish()?;

        Ok(BaseConfig {
            dirs,
            main,
            verbose: false,
        })
    }

    /// Merge global.toml and sandbox.d/<name>.toml at the TOML value level,
    /// deserialize the result once, and consume the base into this
    /// invocation's configuration.
    pub fn load_sandbox(self, name: Name) -> Result<Config> {
        let sandbox = ConfigLoader::new(SANDBOX_CONFIG_APPEND)
            .layer(&self.dirs.global_config_file())?
            .layer(&self.dirs.sandbox_config_file(name.as_str()))?
            .finish()
            .with_context(|| format!("loading the configuration for '{name}'"))?;

        Ok(Config {
            base: self,
            name,
            keep: false,
            no_attach: false,
            sandbox,
        })
    }

    fn validate(&self) -> Result<()> {
        self.main.validate()
    }
}

/// One invocation's configuration: the base it was built from, the sandbox it
/// names, and that sandbox's resolved settings. The name is identity, not a
/// setting, and every Config has one.
///
/// The sandbox settings are whatever the files and the command line say; the
/// layer that uses each setting applies the defaults that depend on the
/// invocation.
#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub base: BaseConfig,
    pub name: Name,
    pub keep: bool,
    pub no_attach: bool,
    pub sandbox: SandboxConfig,
}

impl Config {
    pub fn validate(&self) -> Result<()> {
        self.base.validate()?;
        self.sandbox.validate()?;
        self.check_paths()
    }

    /// Every path that becomes a mount source or destination has to be
    /// absolute: the tree is built after the process has left the host root
    /// and working directory behind, so a relative path would mean something
    /// else there.
    fn check_paths(&self) -> Result<()> {
        let dir = self.sandbox_dir();
        if !dir.is_absolute() {
            bail!("the sandbox directory '{}' must be absolute", dir.display());
        }

        for bind in &self.sandbox.binds {
            let source = self.dirs().expand(&bind.source);
            if !source.is_absolute() {
                bail!("the bind source '{}' must be absolute", bind.source);
            }

            if let Some(dest) = &bind.dest {
                let expanded = self.dirs().expand(dest);
                if !expanded.is_absolute() {
                    bail!("the bind destination '{}' must be absolute", dest);
                }
            }
        }

        Ok(())
    }

    /// The directories sbox works with.
    pub fn dirs(&self) -> &XdgDirs {
        &self.base.dirs
    }

    /// The paths that belong to this sandbox.
    pub fn paths(&self) -> SandboxPaths<'_> {
        self.base.dirs.paths(&self.name)
    }

    /// The sandbox name.
    pub fn name(&self) -> &Name {
        &self.name
    }

    /// The base this configuration was built from.
    pub fn into_base(self) -> BaseConfig {
        self.base
    }

    pub fn verbose(&self) -> bool {
        self.base.verbose
    }

    /// Whether run was asked to start the sandbox without opening a session.
    pub fn no_attach(&self) -> bool {
        self.no_attach
    }

    /// Whether the sandbox outlives its last session (--keep or --no-attach).
    pub fn persistent(&self) -> bool {
        self.keep || self.no_attach
    }

    /// The in-sandbox $HOME: the configured directory, or <sandbox_root>/<name>
    /// from config.toml, or ~/Sandbox/<name>. Every part of it is already in
    /// the configuration, so this answers the question instead of leaving each
    /// caller to spell the fallbacks out again.
    pub fn sandbox_dir(&self) -> PathBuf {
        if !self.sandbox.dir.is_empty() {
            return self.dirs().expand(&self.sandbox.dir);
        }

        match &self.base.main.sandbox_root {
            Some(root) => {
                self.dirs()
                    .expand(&format!("{}/{}", root.trim_end_matches('/'), self.name()))
            }
            None => self
                .dirs()
                .home()
                .join("Sandbox")
                .join(self.name().as_str()),
        }
    }
}

#[cfg(test)]
impl Config {
    /// A configuration over a throwaway home, for tests that do not need a
    /// config.toml.
    pub fn under(home: impl Into<PathBuf>) -> Config {
        let name = Name::parse("test").expect("a valid test name");

        BaseConfig::at(XdgDirs::under(home))
            .and_then(|base| base.load_sandbox(name))
            .expect("a fresh home holds no configuration")
    }
}

#[cfg(test)]
mod tests {
    use super::{BaseConfig, Config, EnvValue, Hostname, PkiMode, XdgDirs};
    use crate::cli::{Cli, Cmd};
    use clap::Parser;

    fn cli(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("sbox").chain(args.iter().copied())).expect("parse")
    }

    fn xdg(tag: &str) -> XdgDirs {
        let base = std::env::temp_dir().join(format!("sbox-config-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        XdgDirs::under(&base)
    }

    fn write(dirs: &XdgDirs, relative: &str, text: &str) {
        let path = dirs.config_home().join("sbox").join(relative);
        std::fs::create_dir_all(path.parent().expect("a file has a parent")).expect("mkdir");
        std::fs::write(path, text).expect("write");
    }

    /// Write the files, then build and resolve a configuration for args.
    fn load(tag: &str, files: &[(&str, &str)], args: &[&str]) -> Config {
        let dirs = xdg(tag);
        for (relative, text) in files {
            write(&dirs, relative, text);
        }
        let cli = cli(args);
        let name = match cli.action() {
            Cmd::Run(args) => args.name.clone(),
            Cmd::Attach(args) => args.name.clone(),
            other => panic!("the test command names a sandbox: {other:?}"),
        };
        let mut config = BaseConfig::at(dirs)
            .expect("load")
            .load_sandbox(name)
            .expect("load sandbox");
        cli.apply(&mut config);
        config
    }

    #[test]
    fn defaults_are_concrete() {
        let config = load("defaults", &[], &["run", "work"]);
        assert!(config.sandbox.shares.x11);
        assert!(config.sandbox.gui.icons);
        assert_eq!(config.sandbox.shares.pki, PkiMode::Ro);
        assert!(!config.sandbox.dbus.raw);
        assert!(!config.sandbox.nested);
        assert_eq!(
            config.sandbox.command.last().map(String::as_str),
            Some("-l")
        );
        assert!(config.sandbox.dbus.session.is_empty());
        assert!(config.sandbox.binds.is_empty());
    }

    #[test]
    fn layers_merge_from_global_to_one_sandbox() {
        let config = load(
            "layers",
            &[
                (
                    "global.toml",
                    "hostname = \"global\"\n[shares]\nx11 = true\n[[bind]]\nsource = \"/one\"\n",
                ),
                (
                    "sandbox.d/work.toml",
                    "[shares]\nx11 = false\n[[bind]]\nsource = \"/two\"\n",
                ),
                ("sandbox.d/other.toml", "hostname = \"other\"\n"),
            ],
            &["run", "work"],
        );

        assert_eq!(config.sandbox.hostname, Hostname::Named("global".into()));
        assert!(!config.sandbox.shares.x11);
        assert_eq!(config.sandbox.binds.len(), 2);
        assert_eq!(config.sandbox.binds[0].source, "/one");
        assert_eq!(config.sandbox.binds[0].mode, super::BindMode::Rw);
        assert_eq!(config.sandbox.binds[1].source, "/two");
    }

    #[test]
    fn a_sandbox_only_reads_its_own_file() {
        let config = load(
            "other",
            &[
                ("sandbox.d/work.toml", "hostname = \"work-name\"\n"),
                ("sandbox.d/other.toml", "hostname = \"other-name\"\n"),
            ],
            &["run", "other"],
        );
        assert_eq!(
            config.sandbox.hostname,
            Hostname::Named("other-name".into())
        );
    }

    #[test]
    fn the_command_is_replaced_not_appended() {
        let config = load(
            "command",
            &[
                ("global.toml", "command = [\"/bin/global\"]\n"),
                ("sandbox.d/work.toml", "command = [\"/bin/sandbox\"]\n"),
            ],
            &["run", "work"],
        );
        assert_eq!(config.sandbox.command, ["/bin/sandbox"]);
    }

    #[test]
    fn the_command_line_wins_over_the_files() {
        let config = load(
            "cli-command",
            &[("global.toml", "command = [\"/bin/configured\"]\n")],
            &["run", "work", "--", "sh", "-c", "echo hi"],
        );
        assert_eq!(config.sandbox.command, ["sh", "-c", "echo hi"]);
    }

    #[test]
    fn the_configured_command_survives_without_one_on_the_command_line() {
        let config = load(
            "configured-command",
            &[("global.toml", "command = [\"/bin/configured\"]\n")],
            &["run", "work"],
        );
        assert_eq!(config.sandbox.command, ["/bin/configured"]);
    }

    #[test]
    fn flags_override_and_lists_append() {
        let config = load(
            "flags",
            &[("global.toml", "[dbus]\nsession = [\"talk=global\"]\n")],
            &[
                "run",
                "-x",
                "-s",
                "/data",
                "--ro-share",
                "/srv",
                "--dbus-session-rule",
                "talk=cli",
                "--pki",
                "isolate",
                "work",
            ],
        );
        assert!(!config.sandbox.shares.x11);
        assert_eq!(config.sandbox.shares.pki, PkiMode::Isolate);
        assert_eq!(config.sandbox.binds.len(), 2);
        assert_eq!(config.sandbox.binds[0].source, "/data");
        assert_eq!(config.sandbox.binds[1].mode, super::BindMode::Ro);

        let session = &config.sandbox.dbus.session;
        let global = session
            .iter()
            .position(|rule| rule == "talk=global")
            .expect("global");
        let cli = session
            .iter()
            .position(|rule| rule == "talk=cli")
            .expect("cli");
        assert!(global < cli, "{session:?}");
    }

    #[test]
    fn the_hostname_is_left_to_the_namespace_layer() {
        let config = load("hostname", &[], &["run", "work"]);
        assert_eq!(config.sandbox.hostname, Hostname::Auto);
    }

    #[test]
    fn the_hostname_flags_override_the_file() {
        let real = load("hostname-real", &[], &["run", "-H", "work"]);
        assert_eq!(real.sandbox.hostname, Hostname::Real);

        let named = load("hostname-named", &[], &["run", "--hostname", "x", "work"]);
        assert_eq!(named.sandbox.hostname, Hostname::Named("x".into()));
    }

    #[test]
    fn the_sandbox_directory_defaults_under_the_home() {
        let config = load("dir-default", &[], &["run", "work"]);
        assert!(config.sandbox.dir.is_empty());
        assert_eq!(
            config.sandbox_dir(),
            config.dirs().home().join("Sandbox/work")
        );
    }

    #[test]
    fn sandbox_root_replaces_the_parent() {
        let config = load(
            "dir-root",
            &[("config.toml", "sandbox_root = \"~/Boxes\"\n")],
            &["run", "work"],
        );
        assert_eq!(
            config.sandbox_dir(),
            config.dirs().home().join("Boxes/work")
        );
    }

    #[test]
    fn a_configured_directory_wins_and_is_expanded() {
        let config = load(
            "dir-set",
            &[("global.toml", "dir = \"~/Own\"\n")],
            &["run", "work"],
        );
        assert_eq!(config.sandbox_dir(), config.dirs().home().join("Own"));
    }

    #[test]
    fn the_directory_flag_wins_over_the_file() {
        let config = load(
            "dir-flag",
            &[
                ("config.toml", "sandbox_root = \"~/Boxes\"\n"),
                ("global.toml", "dir = \"/srv/work\"\n"),
            ],
            &["run", "--sandbox-dir", "/tmp/here", "work"],
        );
        assert_eq!(config.sandbox_dir(), std::path::Path::new("/tmp/here"));
    }

    #[test]
    fn relative_mount_paths_are_rejected() {
        let config = load(
            "relative-bind",
            &[("sandbox.d/work.toml", "[[bind]]\nsource = \"relative\"\n")],
            &["run", "work"],
        );
        let error = config.validate().expect_err("relative bind source");
        assert!(
            format!("{error:#}").contains("must be absolute"),
            "{error:#}"
        );

        let config = load(
            "relative-dir",
            &[],
            &["run", "--sandbox-dir", "relative", "work"],
        );
        let error = config.validate().expect_err("relative sandbox directory");
        assert!(
            format!("{error:#}").contains("must be absolute"),
            "{error:#}"
        );
    }

    #[test]
    fn a_sandbox_can_remove_what_global_sets() {
        let config = load(
            "env-merge",
            &[
                ("global.toml", "[env]\nFOO = \"global\"\nBAR = \"kept\"\n"),
                ("sandbox.d/work.toml", "[env]\nFOO = false\n"),
            ],
            &["run", "work"],
        );
        assert_eq!(config.sandbox.env.get("FOO"), Some(&EnvValue::Remove));
        assert_eq!(
            config.sandbox.env.get("BAR"),
            Some(&EnvValue::Value("kept".into()))
        );
    }

    #[test]
    fn persistence_comes_from_the_run_flags() {
        assert!(load("keep", &[], &["run", "-k", "work"]).persistent());
        assert!(load("detach", &[], &["run", "--no-attach", "work"]).persistent());
        assert!(!load("plain", &[], &["run", "work"]).persistent());
        assert!(!load("attach", &[], &["attach", "work"]).persistent());
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let dirs = xdg("unknown");
        write(&dirs, "sandbox.d/work.toml", "x11 = true\n");
        let base = BaseConfig::at(dirs).expect("load");
        let name = crate::name::Name::parse("work").expect("name");
        assert!(base.load_sandbox(name).is_err());
    }

    #[test]
    fn an_empty_sandbox_root_is_rejected() {
        let dirs = xdg("empty");
        write(&dirs, "config.toml", "sandbox_root = \"  \"\n");
        let base = BaseConfig::at(dirs).expect("load");
        let error = base.validate().expect_err("rejected");
        assert!(error.to_string().contains("sandbox_root"), "{error:#}");
    }

    #[test]
    fn the_base_config_ignores_the_sandbox_files() {
        let dirs = xdg("list");
        write(&dirs, "sandbox.d/broken.toml", "x11 = true\n");
        let base = BaseConfig::at(dirs).expect("load");
        base.validate().expect("only config.toml is read");
    }
}
