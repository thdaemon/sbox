//! Command line parsing.
//!
//! Every invocation names an action: `run` creates the sandbox if it is not
//! running and runs a command in it, `attach` joins a running one, `stop`,
//! `status` and `list` inspect or retire it.
//!
//! Options may appear before or after the name. Everything from the first word
//! of the command onwards is passed on verbatim, so it never has to be escaped;
//! a command whose first word starts with `-` needs `--`:
//!
//!     sbox run demo -- sh -c 'echo hi'

use crate::config::{Bind, BindMode, Config, Hostname, PkiMode};
use crate::name::Name;
use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "sbox",
    version = crate::config::VERSION,
    about = "Unprivileged sandbox bootstrapper with persistent namespaces

Copyright © 2026 Eric Tian <thxdaemon@gmail.com>.
All rights reserved.

This program is free software: you can redistribute it and/or modify it under 
the terms of the GNU Affero General Public License as published by the 
Free Software Foundation, either version 3 of the License, or (at your option) 
any later version.
This program is distributed in the hope that it will be useful, but 
WITHOUT ANY WARRANTY; without even the implied warranty of MERCHANTABILITY or 
FITNESS FOR A PARTICULAR PURPOSE. See the GNU Affero General Public License 
for more details.
You should have received a copy of the GNU Affero General Public License along 
with this program. If not, see <https://www.gnu.org/licenses/>.",
    after_help = "CONFIG FILES:
    $XDG_CONFIG_HOME/sbox/config.toml            program settings
    $XDG_CONFIG_HOME/sbox/global.toml            defaults for every sandbox
    $XDG_CONFIG_HOME/sbox/sandbox.d/<name>.toml  specified sandbox's settings
    
See sbox(1) for details.",
    disable_help_subcommand = true,
    arg_required_else_help = true
)]
pub(crate) struct Cli {
    /// Print the resolved settings and extra diagnostics
    #[arg(short, long, global = true)]
    verbose: bool,
    #[command(subcommand)]
    action: Cmd,
}

#[derive(Subcommand, Debug)]
pub(crate) enum Cmd {
    /// Create the sandbox if needed, then run a command in it
    Run(RunArgs),
    /// Run a command in a sandbox that is already running
    Attach(AttachArgs),
    /// Stop a sandbox
    Stop {
        /// Sandbox name
        #[arg(value_name = "NAME", value_parser = parse_name)]
        name: Name,
    },
    /// Show the state of a sandbox
    Status {
        /// Sandbox name
        #[arg(value_name = "NAME", value_parser = parse_name)]
        name: Name,

        /// Print the state as JSON
        #[arg(short = 'j', long)]
        json: bool,
    },
    /// List the running sandboxes
    List,
}

#[derive(Args, Debug)]
pub(crate) struct RunArgs {
    /// Sandbox name
    #[arg(value_name = "NAME", value_parser = parse_name)]
    pub name: Name,

    /// Use DIR as the in-sandbox $HOME (default: <sandbox_root>/<name>)
    #[arg(long, value_name = "DIR", help_heading = "Sandbox")]
    pub sandbox_dir: Option<String>,

    /// Hostname inside the sandbox
    #[arg(long, value_name = "NAME", help_heading = "Sandbox")]
    pub hostname: Option<String>,

    /// Keep the host hostname
    #[arg(
        short = 'H',
        long,
        help_heading = "Sandbox",
        conflicts_with = "hostname"
    )]
    pub real_hostname: bool,

    /// Map uid/gid 0 into the sandbox
    #[arg(long, help_heading = "Sandbox")]
    pub fakeroot: bool,

    /// Allow nested user namespaces inside
    #[arg(short = 'N', long, help_heading = "Sandbox")]
    pub nested: bool,

    /// Start the sandbox without attaching; implies --keep
    #[arg(long, help_heading = "Sandbox")]
    pub no_attach: bool,

    /// Keep the sandbox running after its last session ends
    #[arg(short = 'k', long, help_heading = "Sandbox")]
    pub keep: bool,

    /// Share DIR read-write
    #[arg(
        short = 's',
        long = "share",
        value_name = "DIR",
        help_heading = "Sharing"
    )]
    pub share: Vec<String>,

    /// Share DIR read-only
    #[arg(long = "ro-share", value_name = "DIR", help_heading = "Sharing")]
    pub ro_share: Vec<String>,

    /// Do not bind the real /etc/resolv.conf
    #[arg(long, help_heading = "Sharing")]
    pub no_bind_resolv_conf: bool,

    /// Do not share X11 sockets and credentials
    #[arg(short = 'x', long = "no-x11", help_heading = "Sharing")]
    pub no_x11: bool,

    /// Do not share DRI nodes
    #[arg(long = "no-dri", help_heading = "Sharing")]
    pub no_dri: bool,

    /// Do not share PipeWire/PulseAudio sockets
    #[arg(long = "no-audio", help_heading = "Sharing")]
    pub no_audio: bool,

    /// Do not share Wayland sockets
    #[arg(long = "no-wayland", help_heading = "Sharing")]
    pub no_wayland: bool,

    /// How to share ~/.pki
    #[arg(long, value_name = "MODE", value_enum, help_heading = "Sharing")]
    pub pki: Option<PkiMode>,

    /// Share the real dbus sockets instead of filtered proxies (dangerous)
    #[arg(short = 'D', long = "share-raw-dbus", help_heading = "D-Bus")]
    pub share_raw_dbus: bool,

    /// xdg-dbus-proxy rule for the session bus, as KIND=RULE
    #[arg(
        short = 'd',
        long = "dbus-session-rule",
        value_name = "KIND=RULE",
        help_heading = "D-Bus"
    )]
    pub dbus_session_rule: Vec<String>,

    /// xdg-dbus-proxy rule for the system bus, as KIND=RULE
    #[arg(
        long = "dbus-system-rule",
        value_name = "KIND=RULE",
        help_heading = "D-Bus"
    )]
    pub dbus_system_rule: Vec<String>,

    /// Do not add the built-in dbus proxy rules
    #[arg(long, help_heading = "D-Bus")]
    pub no_default_dbus_rules: bool,

    /// Show dbus proxy logs
    #[arg(long, help_heading = "D-Bus")]
    pub debug_dbus: bool,

    /// Disable all GUI workarounds
    #[arg(long, help_heading = "GUI")]
    pub no_gui_workarounds: bool,

    /// Do not share user GTK configs
    #[arg(long, help_heading = "GUI")]
    pub no_gtk_config: bool,

    /// Do not share user icons
    #[arg(long, help_heading = "GUI")]
    pub no_icons: bool,

    /// Do not share user fonts
    #[arg(long, help_heading = "GUI")]
    pub no_fonts: bool,

    /// Do not share user fontconfig configs
    #[arg(long, help_heading = "GUI")]
    pub no_fontconfig: bool,

    /// Do not set IBUS_USE_PORTAL=1
    #[arg(long, help_heading = "GUI")]
    pub no_ibus: bool,

    /// Command to run inside the sandbox; everything after NAME, so a
    /// command that starts with "-" follows a "--"
    #[arg(
        value_name = "COMMAND",
        trailing_var_arg = true,
        num_args = 0..,
        help_heading = "Command"
    )]
    command: Vec<String>,
}

#[derive(Args, Debug)]
pub(crate) struct AttachArgs {
    /// Sandbox name
    #[arg(value_name = "NAME", value_parser = parse_name)]
    name: Name,

    /// Command to run inside the sandbox; everything after NAME, so a
    /// command that starts with "-" follows a "--"
    #[arg(
        value_name = "COMMAND",
        trailing_var_arg = true,
        num_args = 0..,
        help_heading = "Command"
    )]
    command: Vec<String>,
}

impl Cli {
    /// Parse the process arguments. Help, version and usage errors exit here,
    /// the way every clap-based command does.
    pub fn parse() -> Cli {
        <Cli as clap::Parser>::parse()
    }

    pub fn sandbox_name(&self) -> Option<&Name> {
        match &self.action {
            Cmd::Run(args) => Some(&args.name),
            Cmd::Attach(args) => Some(&args.name),
            Cmd::Stop { name } | Cmd::Status { name, .. } => Some(name),
            Cmd::List => None,
        }
    }

    /// The action this invocation names, for the module that runs it.
    pub fn into_action(self) -> Cmd {
        self.action
    }

    fn run_args(&self) -> Option<&RunArgs> {
        match &self.action {
            Cmd::Run(args) => Some(args),
            _ => None,
        }
    }

    /// The command named on the command line, if any. It is the last layer
    /// over the default the configuration names.
    fn command(&self) -> Option<Vec<String>> {
        let argv = match &self.action {
            Cmd::Run(args) => args.command.clone(),
            Cmd::Attach(args) => args.command.clone(),
            Cmd::Stop { .. } | Cmd::Status { .. } | Cmd::List => return None,
        };
        Some(argv).filter(|argv| !argv.is_empty())
    }

    /// Overlay the command line on the configuration: flags win over the
    /// files, list-valued flags append. The caller checks the result.
    pub fn apply(&self, config: &mut Config) {
        config.verbose = self.verbose;
        config.keep = false;
        config.no_attach = false;

        if let Some(args) = self.run_args() {
            config.keep = args.keep;
            config.no_attach = args.no_attach;

            if let Some(dir) = &args.sandbox_dir {
                config.sandbox.dir = PathBuf::from(dir);
            }

            if args.real_hostname {
                config.sandbox.hostname = Hostname::Real;
            } else if let Some(hostname) = &args.hostname {
                config.sandbox.hostname = Hostname::Named(hostname.clone());
            }

            if args.fakeroot {
                config.sandbox.fakeroot = true;
            }

            if args.nested {
                config.sandbox.nested = true;
            }

            let shares = &mut config.sandbox.shares;
            if args.no_x11 {
                shares.x11 = false;
            }
            if args.no_wayland {
                shares.wayland = false;
            }
            if args.no_dri {
                shares.dri = false;
            }
            if args.no_audio {
                shares.audio = false;
            }
            if args.no_bind_resolv_conf {
                shares.resolv_conf = false;
            }
            if let Some(pki) = args.pki {
                shares.pki = pki;
            }

            let gui_off = args.no_gui_workarounds;
            let gui = &mut config.sandbox.gui;
            if gui_off || args.no_gtk_config {
                gui.gtk_config = false;
            }
            if gui_off || args.no_icons {
                gui.icons = false;
            }
            if gui_off || args.no_fonts {
                gui.fonts = false;
            }
            if gui_off || args.no_fontconfig {
                gui.fontconfig = false;
            }
            if gui_off || args.no_ibus {
                gui.ibus = false;
            }

            let dbus = &mut config.sandbox.dbus;
            if args.share_raw_dbus {
                dbus.raw = true;
            }
            if args.debug_dbus {
                dbus.debug = true;
            }
            if args.no_default_dbus_rules {
                dbus.no_default_rules = true;
            }
            dbus.session.extend(args.dbus_session_rule.iter().cloned());
            dbus.system.extend(args.dbus_system_rule.iter().cloned());

            for source in &args.share {
                config.sandbox.binds.push(share(source, BindMode::Rw));
            }
            for source in &args.ro_share {
                config.sandbox.binds.push(share(source, BindMode::Ro));
            }
        }

        if let Some(argv) = self.command() {
            config.sandbox.command = argv;
        }
    }
}

fn share(source: &str, mode: BindMode) -> Bind {
    Bind {
        source: source.to_string(),
        dest: None,
        mode,
    }
}

fn parse_name(name: &str) -> Result<Name, String> {
    Name::parse(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(args: &[&str]) -> Cli {
        Cli::try_parse_from(std::iter::once("sbox").chain(args.iter().copied())).expect("parse")
    }

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(std::iter::once("sbox").chain(args.iter().copied()))
    }

    #[test]
    fn run_takes_the_command_verbatim() {
        let cli = cli(&["run", "demo", "sh", "-c", "echo hi"]);
        assert_eq!(cli.sandbox_name().unwrap().as_str(), "demo");
        assert_eq!(cli.command().unwrap(), ["sh", "-c", "echo hi"]);
    }

    #[test]
    fn double_dash_is_dropped() {
        assert_eq!(
            cli(&["run", "demo", "--", "sh", "-c", "echo hi"])
                .command()
                .unwrap(),
            ["sh", "-c", "echo hi"]
        );
        assert!(cli(&["run", "demo"]).command().is_none());
    }

    #[test]
    fn options_come_before_the_command() {
        let cli = cli(&["run", "-x", "-s", "/data", "--ro-share", "/srv", "demo"]);
        assert_eq!(cli.sandbox_name().unwrap().as_str(), "demo");
        assert!(cli.command().is_none());
    }

    #[test]
    fn a_command_may_start_with_a_dash_after_the_separator() {
        assert_eq!(cli(&["run", "demo", "--", "-x"]).command().unwrap(), ["-x"]);
    }

    #[test]
    fn attach_takes_only_the_command() {
        let cli = cli(&["attach", "demo", "--", "sh", "-c", "echo hi"]);
        assert_eq!(cli.sandbox_name().unwrap().as_str(), "demo");
        assert_eq!(cli.command().unwrap(), ["sh", "-c", "echo hi"]);
    }

    #[test]
    fn subcommands() {
        assert!(matches!(cli(&["list"]).into_action(), Cmd::List));
        match cli(&["stop", "demo"]).into_action() {
            Cmd::Stop { name } => assert_eq!(name.as_str(), "demo"),
            other => panic!("{other:?}"),
        }
        match cli(&["status", "demo"]).into_action() {
            Cmd::Status { name, json } => {
                assert_eq!(name.as_str(), "demo");
                assert!(!json);
            }
            other => panic!("{other:?}"),
        }
        match cli(&["status", "-j", "demo"]).into_action() {
            Cmd::Status { json, .. } => assert!(json),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_subcommand_is_required() {
        assert!(parse(&[]).is_err());
        assert!(parse(&["demo"]).is_err());
        assert!(parse(&["run"]).is_err());
    }

    #[test]
    fn invalid_names_are_rejected() {
        assert!(parse(&["run", "bad/name"]).is_err());
        assert!(parse(&["stop", "bad name"]).is_err());
        assert!(parse(&["status", "bad/name"]).is_err());
    }

    #[test]
    fn pki_modes_are_validated() {
        assert!(parse(&["run", "--pki", "isolate", "demo"]).is_ok());
        assert!(parse(&["run", "--pki", "sometimes", "demo"]).is_err());
    }

    #[test]
    fn verbose_is_global() {
        assert!(cli(&["-v", "run", "demo"]).verbose);
        assert!(cli(&["run", "-v", "demo"]).verbose);
        assert!(cli(&["run", "demo", "--verbose"]).verbose);
    }

    #[test]
    fn keep_and_no_attach() {
        match cli(&["run", "-k", "--no-attach", "demo"]).into_action() {
            Cmd::Run(args) => {
                assert!(args.keep);
                assert!(args.no_attach);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn hostname_switches_conflict() {
        assert!(parse(&["run", "-H", "--hostname", "x", "demo"]).is_err());
    }
}

#[cfg(test)]
mod apply_tests {
    use super::Cli;
    use crate::config::{BindMode, Config, Hostname};
    use clap::Parser;

    fn applied(tag: &str, args: &[&str]) -> Config {
        let base = std::env::temp_dir().join(format!("sbox-apply-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let mut config = Config::under(&base);
        let cli = Cli::try_parse_from(std::iter::once("sbox").chain(args.iter().copied()))
            .expect("parse");
        cli.apply(&mut config);
        config
    }

    #[test]
    fn gui_switches() {
        let config = applied("gui-all", &["run", "--no-gui-workarounds", "demo"]);
        assert!(!config.sandbox.gui.gtk_config);
        assert!(!config.sandbox.gui.icons);
        assert!(!config.sandbox.gui.fonts);
        assert!(!config.sandbox.gui.fontconfig);
        assert!(!config.sandbox.gui.ibus);

        let config = applied("gui-icons", &["run", "--no-icons", "demo"]);
        assert!(!config.sandbox.gui.icons);
        assert!(config.sandbox.gui.fonts);
    }

    #[test]
    fn dbus_flags() {
        let config = applied(
            "dbus",
            &[
                "run",
                "--share-raw-dbus",
                "--debug-dbus",
                "--dbus-system-rule",
                "own=org.example",
                "demo",
            ],
        );
        assert!(config.sandbox.dbus.raw);
        assert!(config.sandbox.dbus.debug);
        assert!(
            config
                .sandbox
                .dbus
                .system
                .contains(&"own=org.example".to_string())
        );
    }

    #[test]
    fn run_flags() {
        let config = applied(
            "run",
            &["run", "-k", "--no-attach", "-N", "--fakeroot", "demo"],
        );
        assert!(config.persistent());
        assert!(config.no_attach());
        assert!(config.sandbox.nested);
        assert!(config.sandbox.fakeroot);
    }

    #[test]
    fn attach_carries_only_its_command() {
        let config = applied("attach", &["attach", "demo", "--", "sh", "-c", "echo hi"]);
        assert!(config.sandbox.shares.x11);
        assert!(!config.verbose());
        assert!(!config.persistent());
        assert_eq!(config.sandbox.command, ["sh", "-c", "echo hi"]);
    }

    #[test]
    fn hostname_flags() {
        let real = applied("hostname-real", &["run", "-H", "demo"]);
        assert_eq!(real.sandbox.hostname, Hostname::Real);

        let named = applied("hostname-named", &["run", "--hostname", "x", "demo"]);
        assert_eq!(named.sandbox.hostname, Hostname::Named("x".into()));
    }

    #[test]
    fn shares_become_binds() {
        let config = applied(
            "shares",
            &["run", "-s", "/data", "--ro-share", "/srv", "demo"],
        );
        assert_eq!(config.sandbox.binds.len(), 2);
        assert_eq!(config.sandbox.binds[0].source, "/data");
        assert_eq!(config.sandbox.binds[0].dest, None);
        assert_eq!(config.sandbox.binds[0].mode, BindMode::Rw);
        assert_eq!(config.sandbox.binds[1].mode, BindMode::Ro);
    }

    #[test]
    fn verbose_is_global() {
        for args in [
            &["-v", "run", "demo"][..],
            &["run", "-v", "demo"],
            &["run", "demo", "--verbose"],
        ] {
            assert!(applied("verbose", args).verbose());
        }
    }
}
