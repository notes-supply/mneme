//! `mneme autosave run` and `mneme autosave init`.

use std::fs;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use serde_json::json;
use tokio::signal::unix::{SignalKind, signal};

use super::git::Git;
use super::{Autosave, Config, Outcome, Token};
use crate::log;

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
    #[arg(long, default_value_t = 5.0)]
    pub tick_seconds: f64,
    /// Seconds every changed file must have been still before a save.
    #[arg(long, default_value_t = 15.0)]
    pub quiet_seconds: f64,
    /// Seconds between fetches when nothing else asks for one.
    #[arg(long, default_value_t = 30.0)]
    pub fetch_seconds: f64,
    /// Seconds between reports of a state that has not changed.
    #[arg(long, default_value_t = 300.0)]
    pub report_seconds: f64,
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
        let seconds = |value: Option<f64>, default: u64| {
            value.map_or(Duration::from_secs(default), Duration::from_secs_f64)
        };
        Ok(Config {
            folder: self.folder.clone(),
            server: self.server.trim_end_matches('/').to_owned(),
            token: Token(token.to_owned()),
            machine: self.name.clone(),
            quiet: seconds(run.map(|run| run.quiet_seconds), 15),
            fetch_every: seconds(run.map(|run| run.fetch_seconds), 30),
            report_every: seconds(run.map(|run| run.report_seconds), 300),
        })
    }
}

/// Ticks until SIGTERM or Ctrl-C, and logs each change of outcome.
fn serve(run: &Run) -> Result<()> {
    let config = run.target.config(Some(run))?;
    let pause = Duration::from_secs_f64(run.tick_seconds);
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
    let git = Git::new(&config);
    let url = format!("{}/git/wiki.git", config.server);
    let path = &config.folder;
    let empty = match fs::read_dir(path) {
        Ok(mut entries) => entries.next().is_none(),
        Err(error) if error.kind() == ErrorKind::NotFound => true,
        Err(error) => return Err(error).context(format!("reading {}", path.display())),
    };
    if empty {
        fs::create_dir_all(path)?;
        let run = git.clone_from(&url)?;
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
        git.ok(&["remote", "add", "origin", &url])?;
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
