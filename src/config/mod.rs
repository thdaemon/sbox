//! Layered TOML configuration, all of it under $XDG_CONFIG_HOME/sbox.
//!
//! Two kinds of file: config.toml is the program's own configuration, today
//! only where the sandboxes live (sandbox_root). global.toml and
//! sandbox.d/<name>.toml describe the sandboxes themselves.
//!
//! A Config is built in two steps. Config::load reads the XDG directories and
//! config.toml; Config::load_sandbox merges the sandbox files as TOML values
//! and deserializes the result once. The caller then applies the command line.
//! Defaults that depend on the invocation -- the derived hostname, the sandbox
//! directory, the built-in dbus rules -- belong to the layer that uses them.

mod dirs;
mod load;
mod model;

pub(crate) use dirs::{CONTROL_SOCKET, XdgDirs};
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

/// The configuration of one invocation: where sbox keeps things, and one
/// sandbox's resolved settings.
///
/// The name is identity, not a setting: it is set when the sandbox
/// configuration is loaded, except for `list`, which names none. The sandbox
/// settings are whatever the files and the command line say; the layer that
/// uses each setting applies the defaults that depend on the invocation.
#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub dirs: XdgDirs,
    main: MainConfig,
    pub name: Option<Name>,
    pub verbose: bool,
    pub keep: bool,
    pub no_attach: bool,
    pub sandbox: SandboxConfig,
}

impl Config {
    /// Resolve the XDG directories and read config.toml, the program
    /// configuration.
    pub fn load() -> Result<Config> {
        Config::at(XdgDirs::from_env())
    }

    fn at(dirs: XdgDirs) -> Result<Config> {
        let path = dirs.main_config_file();
        let main: MainConfig = ConfigLoader::new(&[]).layer(&path)?.finish()?;

        Ok(Config {
            dirs,
            main,
            name: None,
            verbose: false,
            keep: false,
            no_attach: false,
            sandbox: SandboxConfig::default(),
        })
    }

    /// Merge global.toml and sandbox.d/<name>.toml at the TOML value level,
    /// deserialize the result once, and adopt both it and the name as this
    /// invocation's state.
    pub fn load_sandbox(&mut self, name: Name) -> Result<()> {
        let sandbox = ConfigLoader::new(SANDBOX_CONFIG_APPEND)
            .layer(&self.dirs.global_config_file())?
            .layer(&self.dirs.sandbox_config_file(name.as_str()))?
            .finish()
            .with_context(|| format!("loading the configuration for '{name}'"))?;

        self.name = Some(name);
        self.sandbox = sandbox;
        Ok(())
    }

    pub fn validate(&self) -> Result<()> {
        self.main.validate()?;
        self.sandbox.validate()?;
        self.check_paths()
    }

    /// Every path that becomes a mount source or destination has to be
    /// absolute: the tree is built after the process has left the host root
    /// and working directory behind, so a relative path would mean something
    /// else there.
    fn check_paths(&self) -> Result<()> {
        if self.name.is_none() {
            return Ok(());
        }

        let dir = self.sandbox_dir();
        if !dir.is_absolute() {
            bail!("the sandbox directory '{}' must be absolute", dir.display());
        }

        for bind in &self.sandbox.binds {
            let source = self.dirs.expand(&bind.source);
            if !source.is_absolute() {
                bail!("the bind source '{}' must be absolute", bind.source);
            }

            if let Some(dest) = &bind.dest {
                let expanded = self.dirs.expand(dest);
                if !expanded.is_absolute() {
                    bail!("the bind destination '{}' must be absolute", dest);
                }
            }
        }

        Ok(())
    }

    /// The directories sbox works with.
    pub fn dirs(&self) -> &XdgDirs {
        &self.dirs
    }

    /// The sandbox name. Every command except list names one, so this is set
    /// whenever a sandbox is involved.
    pub fn name(&self) -> &Name {
        self.name
            .as_ref()
            .expect("the sandbox name is set before it is read")
    }

    pub fn verbose(&self) -> bool {
        self.verbose
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
        if !self.sandbox.dir.as_os_str().is_empty() {
            return self.dirs.expand(&self.sandbox.dir.to_string_lossy());
        }

        match &self.main.sandbox_root {
            Some(root) => {
                self.dirs
                    .expand(&format!("{}/{}", root.trim_end_matches('/'), self.name()))
            }
            None => self.dirs.home().join("Sandbox").join(self.name().as_str()),
        }
    }

    pub fn state_dir(&self) -> PathBuf {
        self.dirs.state_dir(self.name().as_str())
    }
}

#[cfg(test)]
impl Config {
    /// A configuration over a throwaway home, for tests that do not need a
    /// config.toml.
    pub fn under(home: impl Into<PathBuf>) -> Config {
        Config::at(XdgDirs::under(home)).expect("a fresh home holds no configuration")
    }
}

#[cfg(test)]
mod tests {
    use super::{Config, EnvValue, Hostname, PkiMode, XdgDirs};
    use crate::cli::Cli;
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
        let mut config = Config::at(dirs).expect("load");
        let cli = cli(args);
        if let Some(name) = cli.sandbox_name() {
            config.load_sandbox(name.clone()).expect("load sandbox");
        }
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
        assert!(config.sandbox.dir.as_os_str().is_empty());
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
        let mut config = Config::at(dirs).expect("load");
        let name = crate::name::Name::parse("work").expect("name");
        assert!(config.load_sandbox(name).is_err());
    }

    #[test]
    fn an_empty_sandbox_root_is_rejected() {
        let dirs = xdg("empty");
        write(&dirs, "config.toml", "sandbox_root = \"  \"\n");
        let config = Config::at(dirs).expect("load");
        let error = config.validate().expect_err("rejected");
        assert!(error.to_string().contains("sandbox_root"), "{error:#}");
    }

    #[test]
    fn a_name_is_only_needed_for_sandbox_commands() {
        let dirs = xdg("list");
        let config = Config::at(dirs).expect("load");
        assert!(config.name.is_none());
    }
}
