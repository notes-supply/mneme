//! `mneme autosave run` and `mneme autosave init`.

use std::fs::{self, File, TryLockError};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use serde_json::json;
use tokio::signal::unix::{SignalKind, signal};

use super::git::Git;
use super::{Autosave, Login, Outcome, Remote, Settings, Token};
use crate::{git_version, log};

/// Which folder, which remote, and who this machine is.
#[derive(Debug, clap::Args)]
pub struct Target {
    /// The wiki folder.
    #[arg(long)]
    pub folder: PathBuf,
    #[command(flatten)]
    pub remote: RemoteArgs,
    /// This machine's name: in the server's tokens file, and on every autosave commit.
    #[arg(long)]
    pub name: String,
}

/// Where the folder syncs, and how the service logs in.
#[derive(Debug, clap::Args)]
pub struct RemoteArgs {
    #[command(flatten)]
    pub to: To,
    /// A file that holds this machine's token and nothing else. A server needs one. An HTTPS
    /// remote may take one, which is sent as the password of HTTP Basic authentication with the
    /// URL's user name, or `git` when it has none.
    #[arg(long, required_unless_present = "remote")]
    pub token_file: Option<PathBuf>,
}

/// A Mneme server or a Git remote, and never both.
#[derive(Debug, clap::Args)]
#[group(required = true, multiple = false)]
pub struct To {
    /// A Mneme server's base URL, such as `https://mneme.example`. It checks every push and
    /// lists each machine's state.
    #[arg(long)]
    pub server: Option<String>,
    /// A Git remote, as `origin` will name it: a Git host's `https://host/wiki.git`, or a
    /// path or `file://` URL on this machine. SSH, `git://` and remote helpers are refused.
    #[arg(long)]
    pub remote: Option<String>,
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
    /// The settings of `run`, or of `init` when `run` is `None`.
    fn config(&self, run: Option<&Run>) -> Result<Settings> {
        let remote = self.remote.remote()?;
        let timing = |pick: fn(&Run) -> Duration, default: u64| {
            run.map_or(Duration::from_secs(default), pick)
        };
        Ok(Settings {
            folder: service_folder(&self.folder, &remote)?,
            remote,
            machine: self.name.clone(),
            quiet: timing(|run| run.quiet_seconds, 15),
            fetch_every: timing(|run| run.fetch_seconds, 30),
            report_every: timing(|run| run.report_seconds, 300),
            stale_lock: timing(|run| run.stale_lock, 300),
            git: PathBuf::from("git"),
        })
    }
}

impl RemoteArgs {
    /// The remote, with what it logs in with.
    ///
    /// # Errors
    ///
    /// Fails when the token file cannot be read, the remote is not one Mneme syncs with, or
    /// the token does not fit it.
    pub fn remote(&self) -> Result<Remote> {
        let token = self.token_file.as_deref().map(read_token).transpose()?;
        match (&self.to.server, &self.to.remote) {
            (Some(server), _) => Ok(Remote::Server {
                url: server.trim_end_matches('/').to_owned(),
                token: token.context("a server needs --token-file")?,
            }),
            (None, Some(url)) => Remote::git(url, Login { token }),
            (None, None) => bail!("name a --server or a --remote"),
        }
    }
}

/// The folder as the service gets it. For a remote that is not a server it is made absolute,
/// since Git runs in the folder and finds `.git` from there.
fn service_folder(folder: &Path, remote: &Remote) -> Result<PathBuf> {
    if remote.is_server() {
        return Ok(folder.to_owned());
    }
    std::path::absolute(folder).with_context(|| format!("finding {}", folder.display()))
}

/// The token in `file`, which holds it and nothing else.
fn read_token(file: &Path) -> Result<Token> {
    let token = fs::read_to_string(file)
        .with_context(|| format!("reading the token file {}", file.display()))?;
    let token = token.trim();
    if token.is_empty() {
        bail!("the token file {} is empty", file.display());
    }
    Ok(Token(token.to_owned()))
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
    git_version::check(&config.git, "mneme autosave")?;
    let _held = hold(&config.folder)?;
    let pause = run.tick_seconds;
    let started = match &config.remote {
        Remote::Server { url, .. } => json!({
            "folder": config.folder,
            "server": url,
            "machine": config.machine,
        }),
        Remote::Git { url, .. } => json!({
            "folder": config.folder,
            "remote": url,
            "machine": config.machine,
        }),
    };
    log::emit("started", started);
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

/// What making a folder ready did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ready {
    /// The folder was missing or empty, and is a clone now.
    Cloned,
    /// The folder was a clone of the remote already.
    Checked,
    /// A folder of files became a repository, and the service's first save pushes them.
    Adopted,
}

/// `mneme autosave init`, which logs what it did as a JSON line, as the service does.
fn init(target: &Target) -> Result<()> {
    let config = target.config(None)?;
    git_version::check(&config.git, "mneme autosave")?;
    let fields = match ready(&Git::new(&config), false)? {
        Ready::Cloned => json!({ "folder": config.folder, "cloned": true }),
        Ready::Checked => json!({ "folder": config.folder, "cloned": false }),
        Ready::Adopted => json!({ "folder": config.folder, "cloned": false, "adopted": true }),
    };
    log::emit("initialized", fields);
    Ok(())
}

/// Makes the folder ready for the service of `config`, as `mneme autosave init` does, and also
/// takes a folder of files that is not a repository yet when the remote has no `main`, for
/// `mneme init`, which says what it did in a sentence of its own.
///
/// # Errors
///
/// Fails when Git is too old, or the folder cannot be made ready without deleting or
/// overwriting a file.
pub(crate) fn prepare(config: &Settings) -> Result<Ready> {
    git_version::check(&config.git, "mneme init")?;
    ready(&Git::new(config), true)
}

/// Clones into a missing or empty folder, or checks an existing clone. With `adopt`, a folder
/// of files that is not a repository becomes one on `main` when the remote has no `main` yet,
/// so that the first save commits the files and pushes them. Logs nothing: the caller says
/// what happened.
fn ready(git: &Git, adopt: bool) -> Result<Ready> {
    let url = git.url();
    let path = git.folder();
    let empty = match fs::read_dir(path) {
        Ok(mut entries) => entries.next().is_none(),
        Err(error) if error.kind() == ErrorKind::NotFound => true,
        Err(error) => return Err(error).context(format!("reading {}", path.display())),
    };
    if empty {
        let asked = git.ask_branches()?;
        if !asked.success {
            bail!("asking {url} for its branches failed: {}", asked.error());
        }
        let branches: Vec<String> = String::from_utf8_lossy(&asked.stdout)
            .lines()
            .filter_map(|line| line.split_once('\t'))
            .map(|(_, name)| name.trim_start_matches("refs/heads/").to_owned())
            .collect();
        let has_main = branches.iter().any(|branch| branch == "main");
        if !has_main && !branches.is_empty() {
            bail!(
                "{url} has no main branch, only {}. The service syncs main alone, so name the \
                 wiki's branch main on the remote first",
                branches.join(", ")
            );
        }
        fs::create_dir_all(path)?;
        let run = git.clone_server(has_main)?;
        if !run.success {
            bail!("cloning {url} failed: {}", run.error());
        }
        if !git.run(&["rev-parse", "-q", "--verify", "HEAD"])?.success {
            git.ok(&["symbolic-ref", "HEAD", "refs/heads/main"])?;
        }
        return Ok(Ready::Cloned);
    }
    if !git.git_dir().is_dir() {
        if adopt && git.git_dir().symlink_metadata().is_err() {
            return adopt_files(git);
        }
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
    Ok(Ready::Checked)
}

/// Makes a folder of files a repository on `main` with `origin` set, when the remote has no
/// `main` yet. No file is touched.
fn adopt_files(git: &Git) -> Result<Ready> {
    let url = git.url();
    let path = git.folder();
    let asked = git.ask_remote()?;
    if !asked.success {
        bail!("asking {url} for its main failed: {}", asked.error());
    }
    if !asked.stdout.is_empty() {
        bail!(
            "{} has files and is not a Git repository, and {url} already has a main. Move the \
             files out of the folder, run mneme init again to clone the wiki into it, then move \
             them back in, and the service saves them",
            path.display()
        );
    }
    let run = git.init_folder()?;
    if !run.success {
        bail!("git init failed: {}", run.error());
    }
    git.ok(&["remote", "add", "origin", url])?;
    Ok(Ready::Adopted)
}
