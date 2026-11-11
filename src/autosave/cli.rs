//! `mneme autosave run` and `mneme autosave init`.

use std::fs::{self, File, TryLockError};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use serde_json::json;
use tokio::signal::unix::{SignalKind, signal};

use super::git::Git;
use super::{Autosave, Config, Outcome, Token};
use crate::log;

/// The oldest Git the service works with. `git merge-tree --write-tree` came in 2.38 and its
/// `--merge-base` in 2.40, and since 2.45 its inputs may be trees, which the replay needs for a
/// commit with no parent and for a server with no `main` yet.
const OLDEST_GIT: (u32, u32, u32) = (2, 45, 0);

/// Which folder, which server, and who this machine is.
#[derive(Debug, clap::Args)]
pub struct Target {
    /// The wiki folder.
    #[arg(long)]
    pub folder: PathBuf,
    /// The server's base URL, such as `https://mneme.example`.
    #[arg(long)]
    pub server: String,
    /// A file that holds this machine's token and nothing else.
    #[arg(long)]
    pub token_file: PathBuf,
    /// This machine's name in the server's tokens file.
    #[arg(long)]
    pub name: String,
}

/// The service's timing. The defaults suit a person or an agent writing pages.
#[derive(Debug, clap::Args)]
pub struct Run {
    #[command(flatten)]
    pub target: Target,
    /// Seconds between ticks.
    #[arg(long, default_value = "5", value_parser = seconds)]
    pub tick_seconds: Duration,
    /// Seconds every changed file must have been still before a save.
    #[arg(long, default_value = "15", value_parser = seconds)]
    pub quiet_seconds: Duration,
    /// Seconds between fetches when nothing else asks for one.
    #[arg(long, default_value = "30", value_parser = seconds)]
    pub fetch_seconds: Duration,
    /// Seconds between reports of a state that has not changed.
    #[arg(long, default_value = "300", value_parser = seconds)]
    pub report_seconds: Duration,
    /// Seconds `.git/index.lock` may sit unchanged before it counts as a dead command's and
    /// is removed.
    #[arg(long, default_value = "300", value_parser = seconds)]
    pub stale_lock: Duration,
}

/// A number of seconds, which may have a fraction, as a duration.
fn seconds(text: &str) -> Result<Duration, String> {
    let value: f64 = text
        .parse()
        .map_err(|_| format!("{text} is not a number"))?;
    Duration::try_from_secs_f64(value).map_err(|_| format!("{text} is not a number of seconds"))
}

/// What `mneme autosave` does.
#[derive(Debug, clap::Subcommand)]
pub enum Command {
    /// Save, sync and push the folder until SIGTERM.
    Run(Run),
    /// Make the folder ready: clone into it when it is missing or empty, or check that it is a
    /// clone of the server on `main`. Never deletes or overwrites a file.
    Init(Target),
}

/// Runs the command.
///
/// # Errors
///
/// Fails when the token file cannot be read, or `init` cannot make the folder ready.
pub fn main(command: Command) -> Result<()> {
    match command {
        Command::Run(run) => serve(&run),
        Command::Init(target) => init(&target),
    }
}

impl Target {
    fn config(&self, run: Option<&Run>) -> Result<Config> {
        let token = fs::read_to_string(&self.token_file)
            .with_context(|| format!("reading the token file {}", self.token_file.display()))?;
        let token = token.trim();
        if token.is_empty() {
            bail!("the token file {} is empty", self.token_file.display());
        }
        let timing = |pick: fn(&Run) -> Duration, default: u64| {
            run.map_or(Duration::from_secs(default), pick)
        };
        Ok(Config {
            folder: self.folder.clone(),
            server: self.server.trim_end_matches('/').to_owned(),
            token: Token(token.to_owned()),
            machine: self.name.clone(),
            quiet: timing(|run| run.quiet_seconds, 15),
            fetch_every: timing(|run| run.fetch_seconds, 30),
            report_every: timing(|run| run.report_seconds, 300),
            stale_lock: timing(|run| run.stale_lock, 300),
            git: PathBuf::from("git"),
        })
    }
}

/// Refuses a Git older than [`OLDEST_GIT`], before anything is changed.
fn check_git(program: &Path) -> Result<()> {
    let output = Process::new(program)
        .arg("--version")
        .output()
        .with_context(|| format!("running {} --version", program.display()))?;
    let said = String::from_utf8_lossy(&output.stdout);
    let said = said.trim();
    let Some(version) = git_version(said) else {
        bail!("cannot read the Git version from {said:?}");
    };
    if version < OLDEST_GIT {
        let (major, minor, patch) = OLDEST_GIT;
        bail!("{said} is too old: mneme autosave needs Git {major}.{minor}.{patch} or newer");
    }
    Ok(())
}

/// The version in `git version 2.45.2`, `git version 2.39.3 (Apple Git-145)` or
/// `git version 2.45.2.windows.1`.
fn git_version(said: &str) -> Option<(u32, u32, u32)> {
    let number = said
        .strip_prefix("git version ")?
        .split_whitespace()
        .next()?;
    let mut parts = number.split('.').map(|part| {
        let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
        digits.parse().ok()
    });
    let major = parts.next()??;
    let minor = parts.next()??;
    let patch = parts.next().flatten().unwrap_or(0);
    Some((major, minor, patch))
}

/// Takes the folder for this service alone, until the returned file is closed when the service
/// exits. Two services on one folder would each commit and move it under the other.
fn hold(folder: &Path) -> Result<File> {
    let path = folder.join(".git").join("mneme-autosave.lock");
    let file = File::options()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| {
            format!(
                "opening {}. Is {} a clone? Run mneme autosave init first",
                path.display(),
                folder.display()
            )
        })?;
    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => bail!(
            "another mneme autosave run is already saving {}, and one folder takes one service",
            folder.display()
        ),
        Err(TryLockError::Error(error)) => {
            Err(error).with_context(|| format!("locking {}", path.display()))
        }
    }
}

/// Ticks until SIGTERM or Ctrl-C, and logs each change of outcome.
fn serve(run: &Run) -> Result<()> {
    let config = run.target.config(Some(run))?;
    check_git(&config.git)?;
    let _held = hold(&config.folder)?;
    let pause = run.tick_seconds;
    log::emit(
        "started",
        json!({
            "folder": config.folder,
            "server": config.server,
            "machine": config.machine,
        }),
    );
    let mut service = Autosave::new(config);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let mut terminate = signal(SignalKind::terminate()).context("watching for SIGTERM")?;
        let mut last: Option<Outcome> = None;
        loop {
            let outcome = service.tick(SystemTime::now());
            if last.as_ref() != Some(&outcome) {
                log::emit(
                    "autosave",
                    json!({ "outcome": outcome.state(), "detail": outcome.detail() }),
                );
                last = Some(outcome);
            }
            tokio::select! {
                () = tokio::time::sleep(pause) => {}
                _ = terminate.recv() => break,
                _ = tokio::signal::ctrl_c() => break,
            }
        }
        log::emit("stopped", json!({}));
        Ok(())
    })
}

fn init(target: &Target) -> Result<()> {
    let config = target.config(None)?;
    check_git(&config.git)?;
    let git = Git::new(&config);
    let url = git.url();
    let path = &config.folder;
    let empty = match fs::read_dir(path) {
        Ok(mut entries) => entries.next().is_none(),
        Err(error) if error.kind() == ErrorKind::NotFound => true,
        Err(error) => return Err(error).context(format!("reading {}", path.display())),
    };
    if empty {
        fs::create_dir_all(path)?;
        let run = git.clone_server()?;
        if !run.success {
            bail!("cloning {url} failed: {}", run.error());
        }
        if !git.run(&["rev-parse", "-q", "--verify", "HEAD"])?.success {
            git.ok(&["symbolic-ref", "HEAD", "refs/heads/main"])?;
        }
        log::emit("initialized", json!({ "folder": path, "cloned": true }));
        return Ok(());
    }
    if !git.git_dir().is_dir() {
        bail!(
            "{} has files and is not a Git repository. Move them aside, or make it a clone \
             of the wiki first",
            path.display()
        );
    }
    let origin = git.run(&["config", "--get", "remote.origin.url"])?;
    if !origin.success {
        git.ok(&["remote", "add", "origin", url])?;
    } else if String::from_utf8_lossy(&origin.stdout).trim() != url {
        bail!(
            "the folder's origin is {}, not {url}",
            String::from_utf8_lossy(&origin.stdout).trim()
        );
    }
    let branch = git
        .line(&["symbolic-ref", "-q", "HEAD"])
        .unwrap_or_default();
    if branch != "refs/heads/main" {
        bail!("the folder is not on main. Check out main first");
    }
    log::emit("initialized", json!({ "folder": path, "cloned": false }));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_git_version_is_read_from_each_way_git_prints_it() {
        for (said, version) in [
            ("git version 2.56.0", Some((2, 56, 0))),
            ("git version 2.39.3 (Apple Git-145)", Some((2, 39, 3))),
            ("git version 2.45.2.windows.1", Some((2, 45, 2))),
            ("git version 2.45.0-rc0", Some((2, 45, 0))),
            ("git version 3.0", Some((3, 0, 0))),
            ("hub version 2.14", None),
            ("git version unknown", None),
        ] {
            assert_eq!(git_version(said), version, "{said}");
        }
    }
}
