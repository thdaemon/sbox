//! Bringing a sandbox up: the parent side that forks and waits, the child side
//! that claims the name, enters the namespaces and becomes PID 1, and the pipe
//! they report over.

mod readiness;
mod setup;

use crate::config::Config;
use anyhow::{Context, Result, bail};
use nix::sys::signal::{Signal, kill};
use nix::sys::wait::waitpid;
use nix::unistd::{ForkResult, fork};
use std::time::Duration;

use readiness::{Report, channel};
use setup::Setup;

/// How long the setup process may take before the caller gives up on it.
const READY_TIMEOUT: Duration = Duration::from_secs(20);

/// Bring a sandbox into existence: fork the setup process, wait until it
/// reports whether it took the name, and reap it either way.
pub(crate) struct Launcher<'a> {
    config: &'a Config,
}

impl<'a> Launcher<'a> {
    pub fn new(config: &'a Config) -> Launcher<'a> {
        Launcher { config }
    }

    pub fn spawn(self) -> Result<()> {
        // Always let a daemon arbitrate through the lock file: if one is
        // already running it reports "busy" immediately and the caller
        // attaches instead.
        let sandbox_dir = self.config.sandbox_dir();
        std::fs::create_dir_all(&sandbox_dir)
            .with_context(|| format!("mkdir {}", sandbox_dir.display()))?;
        std::fs::create_dir_all(self.config.state_dir())?;

        let (readiness, watch) = channel()?;

        match unsafe { fork() }? {
            ForkResult::Child => {
                drop(watch);

                unsafe {
                    libc::setsid();
                }

                Setup::new(self.config.clone()).run(readiness);
            }
            ForkResult::Parent { child } => {
                drop(readiness);

                let report = watch.wait(READY_TIMEOUT)?;

                // The setup process writes the report itself, or hands the
                // sandbox over to the initializer, which writes it. Either way
                // that process is on its way out, and without this wait it stays
                // a zombie under the caller for the whole session. A timeout is
                // the one case where it may still be alive in setup.
                if report == Report::Timeout {
                    let _ = kill(child, Signal::SIGKILL);
                }
                let _ = waitpid(child, None);

                match report {
                    Report::Ready | Report::Busy => Ok(()),
                    // The setup process prints the reason on this terminal
                    // before it exits or is killed.
                    Report::Failed => bail!("sandbox '{}' did not start", self.config.name()),
                    Report::Timeout => bail!(
                        "sandbox '{}' did not start in time, maybe errors will be logged to {}",
                        self.config.name(),
                        self.config.state_dir().join("daemon.log").display()
                    ),
                }
            }
        }
    }
}
