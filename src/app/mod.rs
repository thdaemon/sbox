//! What a parsed command does.
//!
//! cli.rs parses the command line into Cmd, configuration turns it into the
//! invocation's Config, and this module is the only place that knows how the
//! pieces fit together: the control socket, the supervisor, and a session.

mod client;

use self::client::Client;
use crate::cli::{Cli, Cmd};
use crate::config::Config;
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

/// How often to look again while waiting for a socket to appear or vanish.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

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

/// Why the control socket could not be reached. Only the first is worth
/// retrying: the sandbox is still coming up.
#[derive(Debug)]
enum ConnectError {
    NotRunning,
    Io(std::io::Error),
}

impl ConnectError {
    fn describe(self, name: &Name) -> anyhow::Error {
        match self {
            ConnectError::NotRunning => anyhow::anyhow!("sandbox '{name}' is not running"),
            ConnectError::Io(e) => {
                anyhow::Error::new(e).context(format!("connecting to sandbox '{name}'"))
            }
        }
    }
}

/// The invocation: its configuration and the action to run.
///
/// Building it is the only place that knows the order the configuration is
/// assembled in; every command is a method so nothing has to pass the
/// configuration around again.
pub(crate) struct App {
    config: Config,
    action: Cmd,
}

impl App {
    /// Assemble the invocation: read the configuration, teach it which
    /// sandbox this is, overlay the command line, then check the result.
    pub fn new(cli: Cli) -> Result<App> {
        let mut config = Config::load()?;

        if let Some(name) = cli.sandbox_name() {
            config.load_sandbox(name.clone())?;
        }

        cli.apply(&mut config);

        config.validate()?;

        Ok(App {
            config,
            action: cli.into_action(),
        })
    }

    /// Run the command and report the exit status sbox should leave behind.
    pub fn execute(self) -> Result<i32> {
        match &self.action {
            Cmd::Run(_) => self.run(),
            Cmd::Attach(_) => self.attach(),
            Cmd::Stop { .. } => self.stop(),
            Cmd::Status { json, .. } => self.status(*json),
            Cmd::List => self.list(),
        }
    }

    /// Create the sandbox if needed, then run a command in it.
    fn run(&self) -> Result<i32> {
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

            match self.connect(self.config.name(), START_PATIENCE) {
                Ok(stream) => {
                    let attached = Client::new(stream, HANDSHAKE_TIMEOUT)?
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

    /// Run a command in a sandbox that is already running.
    fn attach(&self) -> Result<i32> {
        if self.config.verbose() {
            eprintln!("{:#?}", self.config);
        }

        // Without a command on the command line, this is the configured one.
        let argv = self.config.sandbox.command.clone();

        let stream = self
            .connect(self.config.name(), Duration::ZERO)
            .map_err(|e| e.describe(self.config.name()))?;

        let attached = Client::new(stream, HANDSHAKE_TIMEOUT)?
            .attach()
            .with_context(|| format!("sandbox '{}'", self.config.name()))?;

        // Joining a sandbox shares the terminal just as much as creating one.
        warn_about_tiocsti(self.config.name());

        let session = Session::join(attached.lease, attached.reply, attached.fds)?;

        session.run(argv)
    }

    /// Stop the sandbox and wait for its control socket to go away.
    fn stop(&self) -> Result<i32> {
        let name = self.config.name();

        let stream = self
            .connect(name, Duration::ZERO)
            .map_err(|e| e.describe(name))?;
        let mut client =
            Client::new(stream, STOP_TIMEOUT).context("setting a timeout on the control socket")?;

        // The acknowledgement is best effort: the daemon unlinks its socket
        // and exits either way, and the loop below waits for that.
        let _ = client.stop();

        let deadline = Instant::now() + STOP_TIMEOUT;
        while self.config.dirs().control_socket(name).exists() {
            if Instant::now() > deadline {
                bail!("sandbox '{name}' did not stop");
            }
            std::thread::sleep(POLL_INTERVAL);
        }

        println!("stopped {name}");

        Ok(0)
    }

    /// Show the state of one sandbox.
    fn status(&self, json: bool) -> Result<i32> {
        let instance = self.status_of(self.config.name())?;

        if json {
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

    /// List the names of the sandboxes that answer on their socket.
    fn list(&self) -> Result<i32> {
        let mut names = Vec::new();

        let Ok(entries) = std::fs::read_dir(self.config.dirs().socket_root()) else {
            return Ok(0);
        };

        for entry in entries.flatten() {
            let Some(dir) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };

            let Ok(name) = Name::parse(&dir) else {
                continue;
            };

            let socket = self.config.dirs().control_socket(&name);
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

    /// Ask one sandbox for its state.
    fn status_of(&self, name: &Name) -> Result<Instance> {
        let stream = self
            .connect(name, Duration::ZERO)
            .map_err(|e| e.describe(name))?;
        let mut client = Client::new(stream, STATUS_TIMEOUT)
            .context("setting a timeout on the control socket")?;

        let reply = client
            .status()
            .with_context(|| format!("sandbox '{name}'"))?;

        Ok(Instance {
            name: name.clone(),
            dir: reply.dir,
            pid: reply.pid,
            started: reply.started,
            sessions: reply.sessions,
        })
    }

    /// Open a control socket, waiting up to the given patience for the daemon
    /// to publish it. Starting a sandbox and connecting to it are two steps,
    /// so a caller that just asked for one has to wait; zero patience means
    /// "answer now", which is what status and stop want.
    fn connect(&self, name: &Name, patience: Duration) -> Result<UnixStream, ConnectError> {
        let path = self.config.dirs().control_socket(name);
        let deadline = Instant::now() + patience;

        loop {
            match UnixStream::connect(&path) {
                Ok(stream) => return Ok(stream),
                Err(e) => {
                    let missing = matches!(
                        e.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    );
                    if !missing {
                        return Err(ConnectError::Io(e));
                    }
                    if Instant::now() >= deadline {
                        return Err(ConnectError::NotRunning);
                    }
                    std::thread::sleep(POLL_INTERVAL);
                }
            }
        }
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
