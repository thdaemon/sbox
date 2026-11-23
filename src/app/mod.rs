// SPDX-FileCopyrightText: 2026 Eric Tian <thxdaemon@gmail.com>
// SPDX-License-Identifier: AGPL-3.0-or-later

//! What a parsed command does.
//!
//! cli.rs parses the command line into Cmd, configuration turns it into the
//! invocation's Config, and this module is the only place that knows how the
//! pieces fit together: the control socket, the supervisor, and a session.
//!
//! Each command is its own item holding only the state it reads. Building the
//! invocation is also what decides how much configuration a command needs:
//! list works from the directories alone, stop and status add the name, and
//! only run and attach resolve a sandbox's settings.

mod client;

use self::client::{Client, ConnectError};
use crate::cli::{Cli, Cmd};
use crate::config::{BaseConfig, Config};
use crate::linux::sys;
use crate::name::Name;
use crate::sandbox::{Launcher, Session};
use anyhow::{Context, Result, bail};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

/// How long to wait for a sandbox that is still coming up.
const START_PATIENCE: Duration = Duration::from_secs(10);

/// How many start-and-connect rounds before giving up on a sandbox.
const START_ATTEMPTS: usize = 3;

/// How long to wait before starting over after losing a race.
const RETRY_DELAY: Duration = Duration::from_millis(100);

/// How long the daemon may take to answer an attach request.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a status query may take before the daemon is considered stuck.
const STATUS_TIMEOUT: Duration = Duration::from_secs(2);

/// How long sbox stop waits for the control socket to disappear.
const STOP_TIMEOUT: Duration = Duration::from_secs(10);

/// How often to look again while the stopped sandbox's socket disappears.
const STOP_INTERVAL: Duration = Duration::from_millis(20);

/// State of a running sandbox, as reported by its daemon over the control
/// socket. A sandbox exists exactly as long as that socket answers, so there
/// is no separate record that could point at a recycled pid.
#[derive(Debug, serde::Serialize)]
struct Instance {
    name: Name,
    dir: String,
    pid: i32,
    started: u64,
    /// Host pids of the clients that currently hold a session.
    sessions: Vec<u32>,
}

/// The invocation, one item per command.
pub(crate) enum App {
    Run(Run),
    Attach(Attach),
    Stop(Stop),
    Status(Status),
    List(List),
}

impl App {
    /// Assemble the invocation, resolving only the configuration its command
    /// reads. This is the only place that knows the order the configuration is
    /// assembled in.
    pub fn new(cli: Cli) -> Result<App> {
        let base = BaseConfig::load()?;

        Ok(match cli.action() {
            Cmd::List => App::List(List { base }),
            Cmd::Stop { name } => App::Stop(Stop {
                base,
                name: name.clone(),
            }),
            Cmd::Status { name, json } => App::Status(Status {
                base,
                name: name.clone(),
                json: *json,
            }),
            Cmd::Run(args) => App::Run(Run {
                config: Self::resolve(base, args.name.clone(), &cli)?,
            }),
            Cmd::Attach(args) => {
                let config = Self::resolve(base, args.name.clone(), &cli)?;
                let name = config.name.clone();
                let command = config.sandbox.command.clone();

                App::Attach(Attach {
                    base: config.into_base(),
                    name,
                    command,
                })
            }
        })
    }

    /// Run the command and report the exit status sbox should leave behind.
    pub fn execute(self) -> Result<i32> {
        match self {
            App::Run(command) => command.execute(),
            App::Attach(command) => command.execute(),
            App::Stop(command) => command.execute(),
            App::Status(command) => command.execute(),
            App::List(command) => command.execute(),
        }
    }

    /// Read one sandbox's configuration, overlay the command line on it, and
    /// check the result.
    fn resolve(base: BaseConfig, name: Name, cli: &Cli) -> Result<Config> {
        let mut config = base.load_sandbox(name)?;
        cli.apply(&mut config);
        config.validate()?;
        Ok(config)
    }
}

/// `run`: create the sandbox if needed, then run a command in it.
pub(crate) struct Run {
    config: Config,
}

impl Run {
    fn execute(&self) -> Result<i32> {
        if self.config.verbose() {
            eprintln!("{:#?}", self.config);
        }

        let argv = self.config.sandbox.command.clone();

        // A sandbox that has just served its last session may be on its way
        // out: it still holds the lock for a moment after its socket is gone,
        // so the caller can win the race only by starting over.
        let mut attempt = 0;
        loop {
            attempt += 1;

            Launcher::new(&self.config).spawn()?;

            if self.config.no_attach() {
                return Ok(0);
            }

            match Client::connect(
                self.config.paths().control_socket(),
                START_PATIENCE,
                HANDSHAKE_TIMEOUT,
            ) {
                Ok(client) => {
                    let attached = client
                        .attach()
                        .with_context(|| format!("sandbox '{}'", self.config.name()))?;
                    // The sandbox shares this terminal, warn before anything runs in it.
                    warn_about_tiocsti(self.config.name());

                    let session = Session::join(attached.lease, attached.reply, attached.fds)?;

                    return session.run(argv.clone());
                }
                Err(ConnectError::NotRunning) if attempt < START_ATTEMPTS => {
                    std::thread::sleep(RETRY_DELAY);
                }
                Err(e) => return Err(e.describe(self.config.name())),
            }
        }
    }
}

/// `attach`: run a command in a sandbox that is already running. Only the
/// base, the name and the command matter here.
#[derive(Debug)]
pub(crate) struct Attach {
    base: BaseConfig,
    name: Name,
    command: Vec<String>,
}

impl Attach {
    fn execute(&self) -> Result<i32> {
        if self.base.verbose {
            eprintln!("{:#?}", self);
        }

        let client = Client::connect(
            self.base.dirs.paths(&self.name).control_socket(),
            Duration::ZERO,
            HANDSHAKE_TIMEOUT,
        )
        .map_err(|e| e.describe(&self.name))?;

        let attached = client
            .attach()
            .with_context(|| format!("sandbox '{}'", self.name))?;

        // Joining a sandbox shares the terminal just as much as creating one.
        warn_about_tiocsti(&self.name);

        let session = Session::join(attached.lease, attached.reply, attached.fds)?;

        session.run(self.command.clone())
    }
}

/// `stop`: retire a sandbox and wait for its control socket to go away.
pub(crate) struct Stop {
    base: BaseConfig,
    name: Name,
}

impl Stop {
    fn execute(&self) -> Result<i32> {
        let paths = self.base.dirs.paths(&self.name);

        let mut client = Client::connect(paths.control_socket(), Duration::ZERO, STOP_TIMEOUT)
            .map_err(|e| e.describe(&self.name))?;

        // The acknowledgement is best effort: the daemon unlinks its socket
        // and exits either way, and the loop below waits for that.
        let _ = client.stop();

        let deadline = Instant::now() + STOP_TIMEOUT;
        while paths.control_socket().exists() {
            if Instant::now() > deadline {
                bail!("sandbox '{}' did not stop", self.name);
            }

            std::thread::sleep(STOP_INTERVAL);
        }

        println!("stopped {}", self.name);

        Ok(0)
    }
}

/// `status`: show the state of one sandbox.
pub(crate) struct Status {
    base: BaseConfig,
    name: Name,
    json: bool,
}

impl Status {
    fn execute(&self) -> Result<i32> {
        let mut client = Client::connect(
            self.base.dirs.paths(&self.name).control_socket(),
            Duration::ZERO,
            STATUS_TIMEOUT,
        )
        .map_err(|e| e.describe(&self.name))?;

        let reply = client
            .status()
            .with_context(|| format!("sandbox '{}'", self.name))?;

        let instance = Instance {
            name: self.name.clone(),
            dir: reply.dir,
            pid: reply.pid,
            started: reply.started,
            sessions: reply.sessions,
        };

        if self.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&instance).context("formatting the sandbox state")?
            );
        } else {
            print!(
                "name:     {}\ndir:      {}\npid:      {}\nstarted:  {}\nsessions: {}\nstate:    running\n",
                instance.name,
                instance.dir,
                instance.pid,
                timestamp(instance.started),
                instance.sessions.len()
            );
        }

        Ok(0)
    }
}

/// `list`: print the names of the sandboxes that answer on their socket.
pub(crate) struct List {
    base: BaseConfig,
}

impl List {
    fn execute(&self) -> Result<i32> {
        let mut names = Vec::new();

        let Ok(entries) = std::fs::read_dir(self.base.dirs.socket_root()) else {
            return Ok(0);
        };

        for entry in entries.flatten() {
            let Some(dir) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };

            let Ok(name) = Name::parse(&dir) else {
                continue;
            };

            let socket = self.base.dirs.paths(&name).control_socket();
            if UnixStream::connect(socket).is_ok() {
                names.push(name);
            }
        }

        names.sort();
        for name in names {
            println!("{name}");
        }

        Ok(0)
    }
}

/// Format epoch seconds as UTC, in the ISO 8601 `YYYY-MM-DD HH:MM:SS` style.
fn timestamp(seconds: u64) -> String {
    let time = seconds as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: gmtime_r fills the tm we hand it and keeps no pointer to it.
    if unsafe { libc::gmtime_r(&time, &mut tm) }.is_null() {
        return seconds.to_string();
    }
    let mut buf = [0u8; 64];
    let format = c"%Y-%m-%d %H:%M:%S";
    // SAFETY: the buffer is writable and at least as large as the format can
    // expand to, and tm is a valid struct tm.
    let len = unsafe { libc::strftime(buf.as_mut_ptr().cast(), buf.len(), format.as_ptr(), &tm) };
    String::from_utf8_lossy(&buf[..len]).into_owned()
}

/// The sandbox shares the terminal this command was started from, so a process
/// inside could inject keystrokes into the shell outside it unless the kernel
/// refuses TIOCSTI. Mirror sandbox.sh: warn and point at luit.
fn warn_about_tiocsti(name: &Name) {
    if !sys::is_tty(0) {
        return;
    }
    let blocked = std::fs::read_to_string("/proc/sys/dev/tty/legacy_tiocsti")
        .map(|v| v.trim() == "0")
        .unwrap_or(false);
    if blocked {
        return;
    }
    eprintln!(
        "sbox: warning: dev.tty.legacy_tiocsti is not 0, or this kernel does not provide it."
    );
    eprintln!("sbox: The sandbox shares your terminal, so a process inside could inject");
    eprintln!("sbox: keystrokes into the shell outside it (CVE-2017-5226).");
    eprintln!("sbox: On Linux >= 6.2 run `sysctl dev.tty.legacy_tiocsti=0`; on older kernels");
    eprintln!("sbox: run `sbox attach {name} luit` to get a private pty.");
    eprintln!();
}

#[cfg(test)]
mod tests {
    use super::timestamp;

    #[test]
    fn timestamps_are_utc() {
        assert_eq!(timestamp(0), "1970-01-01 00:00:00");
        assert_eq!(timestamp(1_793_793_697), "2026-11-04 12:01:37");
        assert_eq!(timestamp(951_782_400), "2000-02-29 00:00:00");
    }
}
