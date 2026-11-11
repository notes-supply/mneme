//! `mneme autosave`: makes a wiki folder behave like a note app with autosave. Writers only
//! edit files. The service commits, fetches, replays and pushes, one tick at a time, and keeps
//! nothing but the repository and what this module remembers between ticks.

mod cli;
mod folder;
mod git;
mod replay;

use std::fmt;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use anyhow::{Result, bail};
use serde_json::json;

use crate::log;
use crate::machines::Report;
pub use cli::{Command, main};
use folder::Folder;
use git::Git;
use replay::Local;

/// The longest detail a report carries, under the server's limit.
const MAX_DETAIL: usize = 1000;

/// A machine's token. Its `Debug` leaves the token out.
#[derive(Clone)]
pub struct Token(pub String);

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(..)")
    }
}

/// What one service works on, and how often.
#[derive(Clone, Debug)]
pub struct Config {
    /// The wiki folder, a clone of the server's repository.
    pub folder: PathBuf,
    /// The server's base URL, such as `https://mneme.example`. Its Git routes are below it.
    pub server: String,
    pub token: Token,
    /// The machine's name, as in the server's tokens file. Autosave commits carry it.
    pub machine: String,
    /// How long every changed file must have been still before a save.
    pub quiet: Duration,
    /// How often to fetch when nothing else asks for it.
    pub fetch_every: Duration,
    /// How often to report the state when it has not changed.
    pub report_every: Duration,
    /// How long `index.lock` may sit unchanged before it counts as a dead command's. A live
    /// command that holds it keeps writing it, and none in a wiki folder writes the index for
    /// minutes.
    pub stale_lock: Duration,
}

/// What a tick did, or why it did nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Idle,
    Saved,
    Pulled,
    Pushed,
    /// The server cannot be reached. The detail is Git's error.
    Offline(String),
    /// The folder is in a state the service must not touch, or the server refused a push.
    Blocked(String),
    /// A save would have deleted most pages, so nothing was committed.
    Stopped(String),
}

impl Outcome {
    /// The state as `POST /git/status` takes it.
    #[must_use]
    pub fn state(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Saved => "saved",
            Self::Pulled => "pulled",
            Self::Pushed => "pushed",
            Self::Offline(_) => "offline",
            Self::Blocked(_) => "blocked",
            Self::Stopped(_) => "stopped",
        }
    }

    /// Why, for an outcome that has a reason. Otherwise empty.
    #[must_use]
    pub fn detail(&self) -> &str {
        match self {
            Self::Idle | Self::Saved | Self::Pulled | Self::Pushed => "",
            Self::Offline(detail) | Self::Blocked(detail) | Self::Stopped(detail) => detail,
        }
    }
}

/// How far a tick got, in the order its steps run.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Progress {
    Idle,
    Saved,
    Pulled,
    Pushed,
}

/// What the service remembers between ticks. Losing it costs one fetch and one report.
#[derive(Default)]
struct State {
    /// When a fetch was last tried, and the head it was tried at.
    fetched: Option<(SystemTime, Option<String>)>,
    /// Git's error, while the server cannot be reached.
    offline: Option<String>,
    /// A push found the server had moved, so the next tick fetches.
    stale: bool,
    /// A head the server's push check refused, with its reason.
    refused: Option<(String, String)>,
    /// The last report the server took, and when.
    reported: Option<(Report, SystemTime)>,
}

/// How local history stands against the server's.
enum Shape {
    Same,
    Behind,
    Ahead,
    /// Local commits to replay, because the server moved too or local history has a merge.
    Diverged(Vec<Local>),
}

enum Save {
    Saved(String),
    Stopped(String),
    Nothing,
}

enum Push {
    Done,
    /// The server moved since the fetch.
    Moved,
    /// The server's push check refused it, for this reason.
    Refused(String),
    Failed(String),
}

/// One service and the little it remembers between ticks.
pub struct Autosave {
    config: Config,
    git: Git,
    http: ureq::Agent,
    state: State,
}

impl Autosave {
    /// A service that has not ticked yet.
    #[must_use]
    pub fn new(config: Config) -> Self {
        let http = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(10)))
            .build()
            .into();
        Self {
            git: Git::new(&config),
            config,
            http,
            state: State::default(),
        }
    }

    /// Does one round of guard, save, fetch, integrate and push at `now`, then reports.
    /// It never fails: what goes wrong is the outcome, and the next tick tries again.
    pub fn tick(&mut self, now: SystemTime) -> Outcome {
        self.git.set_now(now);
        let (outcome, head) = self
            .step(now)
            .unwrap_or_else(|error| (Outcome::Blocked(format!("{error:#}")), None));
        self.report(now, &outcome, head);
        outcome
    }

    fn step(&mut self, now: SystemTime) -> Result<(Outcome, Option<String>)> {
        if let Some(reason) = self.blocked(now)? {
            return Ok((Outcome::Blocked(reason), None));
        }
        let folder = Folder::read(&self.git)?;
        match folder.branch.as_deref() {
            Some("main") => {}
            Some(other) => {
                let reason = format!("HEAD is on {other}, not main");
                return Ok((Outcome::Blocked(reason), folder.head));
            }
            None => {
                let reason = "HEAD is detached, not on main".to_owned();
                return Ok((Outcome::Blocked(reason), folder.head));
            }
        }
        let mut head = folder.head.clone();
        let mut progress = Progress::Idle;
        let mut stopped = None;
        let mut dirty = !folder.clean();
        if dirty && folder.quiet(&self.config.folder, now, self.config.quiet) {
            match self.save(&folder)? {
                Save::Saved(commit) => {
                    head = Some(commit);
                    progress = Progress::Saved;
                    dirty = false;
                }
                Save::Nothing => dirty = false,
                Save::Stopped(reason) => stopped = Some(reason),
            }
        }
        let fetched = self.fetch_due(now, head.as_ref());
        if fetched {
            self.fetch(now, head.clone())?;
        }
        let mut blocked = None;
        if fetched && self.state.offline.is_none() {
            blocked = self.integrate_and_push(&mut head, &mut progress, dirty)?;
            if let Some((_, at)) = &mut self.state.fetched {
                at.clone_from(&head);
            }
        } else if let Some((refused, reason)) = &self.state.refused
            && head.as_ref() == Some(refused)
        {
            blocked = Some(reason.clone());
        }
        let outcome = match (stopped, blocked, &self.state.offline) {
            (Some(reason), _, _) => Outcome::Stopped(reason),
            (None, Some(reason), _) => Outcome::Blocked(reason),
            (None, None, Some(error)) => Outcome::Offline(error.clone()),
            (None, None, None) => match progress {
                Progress::Idle => Outcome::Idle,
                Progress::Saved => Outcome::Saved,
                Progress::Pulled => Outcome::Pulled,
                Progress::Pushed => Outcome::Pushed,
            },
        };
        Ok((outcome, head))
    }

    /// Why the folder must not be touched now, from what Git leaves in `.git`. An
    /// `index.lock` unchanged for longer than the stale time was left by a command that died,
    /// so it goes, and the tick carries on.
    fn blocked(&self, now: SystemTime) -> Result<Option<String>> {
        let git_dir = self.git.git_dir();
        if !git_dir.is_dir() {
            return Ok(Some("the folder is not a Git repository".to_owned()));
        }
        let lock = git_dir.join("index.lock");
        if let Ok(metadata) = lock.symlink_metadata() {
            let age = metadata
                .modified()
                .ok()
                .and_then(|modified| now.duration_since(modified).ok())
                .unwrap_or_default();
            if age <= self.config.stale_lock {
                return Ok(Some(format!(
                    "index.lock is {} s old: another Git command is running, or one was killed",
                    age.as_secs()
                )));
            }
            match std::fs::remove_file(&lock) {
                Err(error) if error.kind() != ErrorKind::NotFound => return Err(error.into()),
                _ => log::emit(
                    "stale lock removed",
                    json!({ "age_seconds": age.as_secs() }),
                ),
            }
            if let Some(reason) = self.repair_index()? {
                return Ok(Some(reason));
            }
        }
        Ok([
            ("rebase-merge", "a rebase is in progress"),
            ("rebase-apply", "a rebase or am is in progress"),
            ("MERGE_HEAD", "a merge is in progress"),
            ("CHERRY_PICK_HEAD", "a cherry-pick is in progress"),
            ("REVERT_HEAD", "a revert is in progress"),
            ("sequencer", "a cherry-pick or revert is in progress"),
            ("BISECT_START", "a bisect is in progress"),
        ]
        .into_iter()
        .find(|(name, _)| git_dir.join(name).exists())
        .map(|(_, reason)| reason.to_owned()))
    }

    /// Reads the index again from `HEAD` when the command that died left it unreadable. Only
    /// the index changes, so every edit in the folder stays and shows as a change to save.
    fn repair_index(&self) -> Result<Option<String>> {
        if Folder::read(&self.git).is_ok() {
            return Ok(None);
        }
        let run = if self.head()?.is_some() {
            self.git.run(&["read-tree", "HEAD"])?
        } else {
            self.git.run(&["read-tree", "--empty"])?
        };
        if !run.success {
            return Ok(Some(run.error()));
        }
        self.refresh_index()?;
        log::emit("index rebuilt", json!({ "from": "HEAD" }));
        Ok(None)
    }

    /// Stores what the files look like in the index. The service's own `git status` does not,
    /// so that it never takes the index's lock from under a writer, and a checkout refuses to
    /// move a file whose entry is out of date even when its text is unchanged.
    fn refresh_index(&self) -> Result<()> {
        self.git.ok(&["update-index", "-q", "--refresh"])?;
        Ok(())
    }

    /// Commits everything, unless that would delete more than half of the pages and more
    /// than five.
    fn save(&self, folder: &Folder) -> Result<Save> {
        let lost = folder.pages_lost();
        if lost > 5 {
            let tracked = match &folder.head {
                Some(head) => self
                    .git
                    .ok(&["ls-tree", "-r", "-z", "--name-only", head])?
                    .split(|&b| b == 0)
                    .filter(|path| folder::is_page(path))
                    .count(),
                None => 0,
            };
            if lost * 2 > tracked {
                return Ok(Save::Stopped(format!(
                    "the save would delete {lost} of {tracked} pages"
                )));
            }
        }
        self.git.ok(&["add", "--all"])?;
        if self.git.run(&["diff", "--cached", "--quiet"])?.success {
            return Ok(Save::Nothing);
        }
        let machine = self.config.machine.as_str();
        let message = format!("Autosave from {machine}");
        let email = git::email(machine);
        let author = [
            ("GIT_AUTHOR_NAME", machine),
            ("GIT_AUTHOR_EMAIL", email.as_str()),
            ("GIT_AUTHOR_DATE", self.git.date()),
        ];
        self.git.ok_with(
            &["commit", "--quiet", "--no-verify", "-m", &message],
            &author,
            &[],
        )?;
        Ok(Save::Saved(self.git.line(&["rev-parse", "HEAD"])?))
    }

    /// On the first tick, when the head moved since the last fetch, after the server moved
    /// under a push, and otherwise once per interval.
    fn fetch_due(&self, now: SystemTime, head: Option<&String>) -> bool {
        match &self.state.fetched {
            None => true,
            Some((at, at_head)) => {
                self.state.stale || at_head.as_ref() != head || *at + self.config.fetch_every <= now
            }
        }
    }

    fn fetch(&mut self, now: SystemTime, head: Option<String>) -> Result<()> {
        self.state.fetched = Some((now, head));
        self.state.stale = false;
        let run = self.git.run(&[
            "fetch",
            "--quiet",
            "--no-tags",
            "--no-write-fetch-head",
            "origin",
            "+refs/heads/*:refs/remotes/origin/*",
        ])?;
        self.state.offline = (!run.success).then(|| run.error());
        Ok(())
    }

    fn origin(&self) -> Result<Option<String>> {
        let run = self
            .git
            .run(&["rev-parse", "-q", "--verify", "refs/remotes/origin/main"])?;
        Ok(run
            .success
            .then(|| String::from_utf8_lossy(&run.stdout).trim().to_owned()))
    }

    fn head(&self) -> Result<Option<String>> {
        let run = self.git.run(&["rev-parse", "-q", "--verify", "HEAD"])?;
        Ok(run
            .success
            .then(|| String::from_utf8_lossy(&run.stdout).trim().to_owned()))
    }

    fn shape(&self, head: Option<&str>, origin: Option<&str>) -> Result<Shape> {
        let range = match (head, origin) {
            (None, None) => return Ok(Shape::Same),
            (None, Some(_)) => return Ok(Shape::Behind),
            (Some(head), Some(origin)) if head == origin => return Ok(Shape::Same),
            (Some(head), None) => head.to_owned(),
            (Some(head), Some(origin)) => format!("{origin}...{head}"),
        };
        let mut args = vec!["rev-list", "--topo-order", "--reverse", "--parents"];
        if origin.is_some() {
            args.push("--left-right");
        }
        args.push(&range);
        let listing = self.git.ok(&args)?;
        let listing = String::from_utf8_lossy(&listing);
        let (mut behind, mut merges, mut local) = (false, false, Vec::new());
        for line in listing.lines() {
            let line = match (origin, line.split_at(1)) {
                (None, _) => line,
                (Some(_), ("<", _)) => {
                    behind = true;
                    continue;
                }
                (Some(_), (">", rest)) => rest,
                _ => bail!("unexpected rev-list line {line:?}"),
            };
            let mut ids = line.split(' ').map(str::to_owned);
            let id = ids.next().unwrap_or_default();
            let parents: Vec<String> = ids.collect();
            if parents.len() > 1 {
                merges = true;
            } else {
                let parent = parents.into_iter().next();
                local.push(Local { id, parent });
            }
        }
        Ok(if merges || (behind && !local.is_empty()) {
            Shape::Diverged(local)
        } else if behind {
            Shape::Behind
        } else {
            Shape::Ahead
        })
    }

    /// Brings in the server's commits and sends the local ones, right after a fetch so that
    /// a replay is never built on an old view of the server. Returns the push check's refusal
    /// if there was one.
    fn integrate_and_push(
        &mut self,
        head: &mut Option<String>,
        progress: &mut Progress,
        dirty: bool,
    ) -> Result<Option<String>> {
        let origin = self.origin()?;
        let ahead = match self.shape(head.as_deref(), origin.as_deref())? {
            Shape::Same => false,
            Shape::Behind => {
                let Some(target) = &origin else {
                    return Ok(None);
                };
                if !self.fast_forward(target)? {
                    return Ok(None);
                }
                head.clone_from(&origin);
                *progress = (*progress).max(Progress::Pulled);
                false
            }
            Shape::Ahead => true,
            Shape::Diverged(commits) => {
                if dirty || !self.integrate(head.as_deref(), origin.as_deref(), &commits)? {
                    return Ok(None);
                }
                *head = self.head()?;
                *progress = (*progress).max(Progress::Pulled);
                *head != origin
            }
        };
        let Some(local) = head.clone().filter(|_| ahead) else {
            return Ok(None);
        };
        if let Some((refused, reason)) = &self.state.refused
            && *refused == local
        {
            return Ok(Some(reason.clone()));
        }
        Ok(match self.push(&local, origin.as_deref())? {
            Push::Done => {
                self.state.refused = None;
                *progress = Progress::Pushed;
                None
            }
            Push::Moved => {
                self.state.stale = true;
                None
            }
            Push::Refused(reason) => {
                self.state.refused = Some((local, reason.clone()));
                Some(reason)
            }
            Push::Failed(error) => {
                self.state.offline = Some(error);
                None
            }
        })
    }

    /// Takes the server's commits. Git refuses when that would overwrite a file with unsaved
    /// edits, which is right: they are saved first, and the next tick replays them.
    fn fast_forward(&self, origin: &str) -> Result<bool> {
        self.refresh_index()?;
        let run = self.git.run(&["merge", "--quiet", "--ff-only", origin])?;
        self.moved_or_waiting(&run, "merge --ff-only")
    }

    /// Replays the local commits onto the server's `main` and moves the folder there.
    fn integrate(
        &self,
        head: Option<&str>,
        origin: Option<&str>,
        commits: &[Local],
    ) -> Result<bool> {
        let Some(head) = head else {
            return Ok(false);
        };
        let Some(tip) = replay::replay(&self.git, origin, commits)?.tip else {
            return Ok(false);
        };
        if self.head()?.as_deref() != Some(head) {
            return Ok(false);
        }
        self.refresh_index()?;
        let run = self.git.run(&["reset", "--quiet", "--keep", &tip])?;
        self.moved_or_waiting(&run, "reset --keep")
    }

    /// Whether a checkout moved the folder. When it did not because the folder has edits in
    /// the way, the service waits for them to be saved. Any other failure is an error.
    fn moved_or_waiting(&self, run: &git::Run, what: &str) -> Result<bool> {
        if run.success {
            return Ok(true);
        }
        if Folder::read(&self.git)?.clean() {
            bail!("git {what} failed: {}", run.error());
        }
        Ok(false)
    }

    /// Pushes `head` over `origin`, the server's main as the last fetch saw it. A push the
    /// server turns down was refused only when its main is still `origin`. When it has moved,
    /// whether Git noticed before sending or the server only when it took the ref's lock, the
    /// push lost a race, and the next tick fetches and replays.
    fn push(&self, head: &str, origin: Option<&str>) -> Result<Push> {
        let refspec = format!("{head}:refs/heads/main");
        let run = self
            .git
            .run(&["push", "--porcelain", "--no-verify", "origin", &refspec])?;
        if run.success {
            return Ok(Push::Done);
        }
        let stdout = String::from_utf8_lossy(&run.stdout);
        let Some(line) = stdout
            .lines()
            .find(|line| line.starts_with('!') && line.contains("refs/heads/main"))
        else {
            return Ok(Push::Failed(run.error()));
        };
        let listing = self.git.run(&["ls-remote", "origin", "refs/heads/main"])?;
        if !listing.success {
            return Ok(Push::Failed(listing.error()));
        }
        let listing = String::from_utf8_lossy(&listing.stdout);
        if listing.split_whitespace().next() != origin {
            return Ok(Push::Moved);
        }
        let remote: Vec<&str> = run
            .stderr
            .lines()
            .filter_map(|line| line.strip_prefix("remote: "))
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .collect();
        Ok(Push::Refused(if remote.is_empty() {
            line.rsplit('\t').next().unwrap_or(line).to_owned()
        } else {
            remote.join("; ")
        }))
    }

    /// Tells the server the state when it changed, and at least once per interval. A report
    /// that does not arrive is sent again on the next tick.
    fn report(&mut self, now: SystemTime, outcome: &Outcome, head: Option<String>) {
        let mut detail = outcome.detail().to_owned();
        if detail.len() > MAX_DETAIL {
            let cut = (0..=MAX_DETAIL)
                .rev()
                .find(|&at| detail.is_char_boundary(at))
                .unwrap_or(0);
            detail.truncate(cut);
        }
        let report = Report {
            state: outcome.state().to_owned(),
            detail,
            head,
        };
        let due = match &self.state.reported {
            None => true,
            Some((last, at)) => *last != report || *at + self.config.report_every <= now,
        };
        if due && self.send(&report).is_ok() {
            self.state.reported = Some((report, now));
        }
    }

    fn send(&self, report: &Report) -> Result<()> {
        let body = serde_json::to_vec(report)?;
        self.http
            .post(format!("{}/git/status", self.config.server))
            .header("authorization", format!("Bearer {}", self.config.token.0))
            .header("content-type", "application/json")
            .send(&body[..])?;
        Ok(())
    }
}
