//! `mneme autosave`: makes a wiki folder behave like a note app with autosave. Writers only
//! edit files. The service commits, fetches, replays and pushes, one tick at a time, and keeps
//! nothing but the repository and what this module remembers between ticks.

mod cli;
mod folder;
mod git;
mod remote;
mod replay;
mod save;

use std::collections::HashSet;
use std::fmt;
use std::io::ErrorKind;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use anyhow::{Result, bail};
use serde_json::json;

use crate::log;
use crate::machines::{MAX_REPORT, MAX_SKIPPED, REPORT_HEADER, Report, Skipped};
pub use cli::{Command, RemoteArgs, To, main};
use folder::{Folder, Leave};
use git::Git;
pub use remote::{Login, Remote, Transport};
use replay::Local;
use save::{Left, Save};

/// The longest detail a report carries, under the server's limit.
const MAX_DETAIL: usize = 1000;
/// How many commits may land on `main` while the service moves the folder, before it leaves
/// the move to the next tick.
const MOVES: usize = 4;
/// What the reflog of `main` says of a move.
const MOVE_MESSAGE: &str = "autosave: move to the server's main";

/// A machine's token. Its `Debug` leaves the token out.
#[derive(Clone)]
pub struct Token(pub String);

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(..)")
    }
}

/// What one service works on, where it syncs, and how often.
#[derive(Clone, Debug)]
pub struct Settings {
    /// The wiki folder, a clone of the remote's repository.
    pub folder: PathBuf,
    pub remote: Remote,
    /// The machine's name, as in a server's tokens file. Autosave commits carry it.
    pub machine: String,
    /// How long every changed file must have been still before a save.
    pub quiet: Duration,
    /// How often to fetch when nothing else asks for it.
    pub fetch_every: Duration,
    /// How often to report the state to a server when it has not changed.
    pub report_every: Duration,
    /// How long `index.lock` may sit unchanged before it counts as a dead command's. A live
    /// command that holds it keeps writing it, and none in a wiki folder writes the index for
    /// minutes.
    pub stale_lock: Duration,
    /// The Git program, `git` on `PATH` unless a test puts a wrapper in its place.
    pub git: PathBuf,
}

/// What one service works on, and how often, when it syncs with a Mneme server. It is
/// [`Settings`] with [`Remote::Server`], as `--server` gives it.
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
    /// How long `index.lock` may sit unchanged before it counts as a dead command's.
    pub stale_lock: Duration,
    /// The Git program, `git` on `PATH` unless a test puts a wrapper in its place.
    pub git: PathBuf,
}

impl From<Config> for Settings {
    fn from(config: Config) -> Self {
        Self {
            folder: config.folder,
            remote: Remote::Server {
                url: config.server,
                token: config.token,
            },
            machine: config.machine,
            quiet: config.quiet,
            fetch_every: config.fetch_every,
            report_every: config.report_every,
            stale_lock: config.stale_lock,
            git: config.git,
        }
    }
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
    /// The state as a report names it.
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
    /// The head when the last tick ended.
    last_head: Option<String>,
    /// The head at which the index last had its file stats stored.
    refreshed: Option<String>,
    /// A head the server's push check refused, with its reason.
    refused: Option<(String, String)>,
    /// A head the folder could not move from, and the left-out paths the server changed.
    stuck: Option<(Option<String>, Vec<Vec<u8>>)>,
    /// New files whose names Git would not stage, so that a save does not try them each tick.
    unstorable: HashSet<Vec<u8>>,
    /// The last report the server took, and when.
    reported: Option<(Report, SystemTime)>,
    /// What the log last said the service leaves out, for a remote that takes no reports.
    logged_skipped: Vec<Skipped>,
}

/// How local history stands against the server's.
enum Shape {
    Same,
    Behind,
    Ahead,
    /// Local commits to replay, because the server moved too or local history has a merge.
    Diverged(Vec<Local>),
}

/// What a tick saw of the folder, for its report.
#[derive(Default)]
struct Seen {
    /// Whether the tick read the folder, which a guard can stop it from doing.
    looked: bool,
    /// The folder's head, or `None` when a guard stopped the tick before reading it.
    head: Option<String>,
    /// What the service leaves in the folder, by path.
    left: Vec<Left>,
}

/// What came of moving the folder.
enum Move {
    /// `main` and the folder are at this commit.
    Done(String),
    /// An edit in the folder is in the way, so nothing moved.
    Waiting,
    /// The server changed these files, which the service leaves out, so nothing moved.
    Stuck(Vec<Left>),
    /// Commits kept landing on `main` while the folder moved, for this reason.
    Blocked(String),
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
    config: Settings,
    git: Git,
    state: State,
}

impl Autosave {
    /// A service that has not ticked yet.
    #[must_use]
    pub fn new(config: impl Into<Settings>) -> Self {
        let config = config.into();
        Self {
            git: Git::new(&config),
            config,
            state: State::default(),
        }
    }

    /// Does one round of guard, save, fetch, integrate and push at `now`, then reports.
    /// It never fails: what goes wrong is the outcome, and the next tick tries again.
    pub fn tick(&mut self, now: SystemTime) -> Outcome {
        self.git.set_now(now);
        let (outcome, seen) = self
            .step(now)
            .unwrap_or_else(|error| (Outcome::Blocked(format!("{error:#}")), Seen::default()));
        self.report(now, &outcome, seen);
        outcome
    }

    fn step(&mut self, now: SystemTime) -> Result<(Outcome, Seen)> {
        if let Some(reason) = self.blocked(now)? {
            return Ok((Outcome::Blocked(reason), Seen::default()));
        }
        let mut folder = Folder::read(&self.git)?;
        let unstorable = &mut self.state.unstorable;
        unstorable.retain(|name| folder.changes.iter().any(|changed| changed.path == *name));
        folder.leave_unstorable(unstorable);
        let mut left = save::left(&folder);
        let reason = match folder.branch.as_deref() {
            Some("main") => None,
            Some(other) => Some(format!("HEAD is on {other}, not main")),
            None => Some("HEAD is detached, not on main".to_owned()),
        };
        if let Some(reason) = reason {
            let seen = Seen {
                looked: true,
                head: folder.head,
                left,
            };
            return Ok((Outcome::Blocked(reason), seen));
        }
        if folder.head.is_some()
            && folder.head == self.state.last_head
            && self.state.refreshed != folder.head
        {
            let run = self.git.run(&["update-index", "-q", "--refresh"])?;
            if run.success {
                self.state.refreshed.clone_from(&folder.head);
            }
        }
        let mut head = folder.head.clone();
        let mut progress = Progress::Idle;
        let mut stopped = None;
        let mut pending = folder.pending();
        if pending && folder.quiet(now, self.config.quiet) {
            let (saved, refused) = save::save(&self.git, &self.config.machine, &folder)?;
            for refused in &refused {
                if refused.leave == Leave::Unstorable {
                    self.state.unstorable.insert(refused.path.clone());
                }
            }
            left.extend(refused);
            match saved {
                Save::Saved(commit) => {
                    head = Some(commit);
                    progress = Progress::Saved;
                    pending = false;
                }
                Save::Nothing => pending = false,
                Save::Stopped(reason) => stopped = Some(reason),
            }
        }
        let fetched = self.fetch_due(now, head.as_ref());
        if fetched {
            self.fetch(now, head.clone())?;
        }
        let mut blocked = None;
        if fetched && self.state.offline.is_none() {
            blocked = self.integrate_and_push(&mut head, &mut progress, pending)?;
            if let Some((_, at)) = &mut self.state.fetched {
                at.clone_from(&head);
            }
        } else if let Some((refused, reason)) = &self.state.refused
            && head.as_ref() == Some(refused)
        {
            blocked = Some(reason.clone());
        } else if let Some((at, paths)) = &self.state.stuck
            && *at == head
        {
            let still: Vec<&Left> = left
                .iter()
                .filter(|left| paths.contains(&left.path))
                .collect();
            if !still.is_empty() {
                blocked = Some(stuck_reason(&still));
            }
        }
        self.state.last_head.clone_from(&head);
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
        left.sort_by(|a, b| a.path.cmp(&b.path));
        left.dedup_by(|a, b| a.path == b.path);
        let seen = Seen {
            looked: true,
            head,
            left,
        };
        Ok((outcome, seen))
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
    /// so that it never takes the index's lock from under a writer. A checkout refuses to move a
    /// file whose entry is out of date even when its text is unchanged, and every `git status`
    /// reads again each page whose entry was written in the same second as the page. So the
    /// service stores them before it moves the folder, and once more when the head has stayed
    /// put for a whole tick, so that the second in which the pages were written has passed.
    fn refresh_index(&self) -> Result<()> {
        self.git.ok(&["update-index", "-q", "--refresh"])?;
        Ok(())
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
        pending: bool,
    ) -> Result<Option<String>> {
        self.state.stuck = None;
        let origin = self.origin()?;
        let ahead = match self.shape(head.as_deref(), origin.as_deref())? {
            Shape::Same => false,
            Shape::Ahead => true,
            shape => {
                let target = self.target(shape, head.as_deref(), origin.as_deref(), pending)?;
                let Some(target) = target else {
                    return Ok(None);
                };
                match self.advance(head.as_deref(), target, origin.as_deref())? {
                    Move::Done(moved) => {
                        *progress = (*progress).max(Progress::Pulled);
                        let ahead = Some(&moved) != origin.as_ref();
                        *head = Some(moved);
                        ahead
                    }
                    Move::Waiting => return Ok(None),
                    Move::Stuck(left) => {
                        let reason = stuck_reason(&left.iter().collect::<Vec<_>>());
                        let paths = left.into_iter().map(|left| left.path).collect();
                        self.state.stuck = Some((head.clone(), paths));
                        return Ok(Some(reason));
                    }
                    Move::Blocked(reason) => return Ok(Some(reason)),
                }
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

    /// Where the folder moves to: the server's main when local history is only behind it, or
    /// the local commits replayed onto it. `None` while unsaved edits wait, which a replay
    /// would leave behind, or when a commit landed during the replay, which the next tick
    /// replays with the rest.
    fn target(
        &self,
        shape: Shape,
        head: Option<&str>,
        origin: Option<&str>,
        pending: bool,
    ) -> Result<Option<String>> {
        match shape {
            Shape::Behind => Ok(origin.map(str::to_owned)),
            Shape::Diverged(commits) => {
                let Some(head) = head.filter(|_| !pending) else {
                    return Ok(None);
                };
                let Some(tip) = replay::replay(&self.git, origin, &commits)?.tip else {
                    return Ok(None);
                };
                Ok((self.head()?.as_deref() == Some(head)).then_some(tip))
            }
            Shape::Same | Shape::Ahead => Ok(None),
        }
    }

    /// Moves the folder and then `main` from `head` to `target`. The folder moves with Git's
    /// two-way merge from `head`, `git read-tree -m -u`, the rule `git reset --keep` uses: it
    /// keeps every edit and staged change, and refuses rather than overwrite one. `main` then
    /// moves only if it is still `head`, with `git update-ref`. A commit that landed on `main`
    /// in between, such as an agent's hand commit, makes that fail. It is replayed onto
    /// `target` and the move tried again from there, so no commit is ever moved off `main`.
    fn advance(&self, head: Option<&str>, target: String, origin: Option<&str>) -> Result<Move> {
        let mut from = head.map(str::to_owned);
        let mut expected = from.clone();
        let mut target = target;
        for round in 0..MOVES {
            self.refresh_index()?;
            let base = from.as_deref().unwrap_or(replay::EMPTY_TREE);
            let run = self.git.run(&["read-tree", "-m", "-u", base, &target])?;
            if !run.success {
                if round > 0 {
                    let landed = expected.as_deref().unwrap_or_default();
                    return Ok(Move::Blocked(format!(
                        "commit {landed} landed on main as the folder moved, and then git \
                         read-tree failed: {}",
                        run.error()
                    )));
                }
                let folder = Folder::read(&self.git)?;
                if folder.clean() {
                    bail!("git read-tree -m -u failed: {}", run.error());
                }
                let stuck = self.left_in_the_way(&folder, base, &target)?;
                return Ok(if stuck.is_empty() {
                    Move::Waiting
                } else {
                    Move::Stuck(stuck)
                });
            }
            let old = expected.as_deref().unwrap_or(git::NO_OBJECT);
            let args = [
                "update-ref",
                "-m",
                MOVE_MESSAGE,
                "refs/heads/main",
                &target,
                old,
            ];
            let swap = self.git.run(&args)?;
            if swap.success {
                return Ok(Move::Done(target));
            }
            let now = self.head()?;
            let Some(landed) = now.filter(|now| Some(now) != expected.as_ref()) else {
                bail!("git update-ref failed: {}", swap.error());
            };
            let commits = self.landed(&landed, expected.as_deref(), origin)?;
            let next = replay::replay(&self.git, Some(&target), &commits)?
                .tip
                .unwrap_or_else(|| target.clone());
            from = Some(std::mem::replace(&mut target, next));
            expected = Some(landed);
        }
        Ok(Move::Blocked(format!(
            "commits kept landing on main as the folder moved; the last was {}",
            expected.unwrap_or_default()
        )))
    }

    /// The files the service leaves out at paths that differ between `from` and `to`. Git's
    /// two-way merge refuses to overwrite them, and no save will ever take them, so the folder
    /// cannot move until a person fixes them.
    fn left_in_the_way(&self, folder: &Folder, from: &str, to: &str) -> Result<Vec<Left>> {
        let changed = self.git.ok(&[
            "diff-tree",
            "-r",
            "-z",
            "--name-only",
            "--no-renames",
            from,
            to,
        ])?;
        let changed: Vec<&[u8]> = changed
            .split(|&b| b == 0)
            .filter(|path| !path.is_empty())
            .collect();
        Ok(save::left(folder)
            .into_iter()
            .filter(|left| {
                changed.iter().any(|path| {
                    *path == left.path
                        || (left.path.ends_with(b"/") && path.starts_with(&left.path))
                })
            })
            .collect())
    }

    /// The commits on `main` at `now` that are neither in `before` nor on the server, oldest
    /// first. Merges are left out, as when local history is replayed.
    fn landed(&self, now: &str, before: Option<&str>, origin: Option<&str>) -> Result<Vec<Local>> {
        let excluded: Vec<String> = [before, origin]
            .into_iter()
            .flatten()
            .map(|id| format!("^{id}"))
            .collect();
        let mut args = vec![
            "rev-list",
            "--topo-order",
            "--reverse",
            "--parents",
            "--no-merges",
        ];
        args.push(now);
        args.extend(excluded.iter().map(String::as_str));
        let listing = self.git.ok(&args)?;
        Ok(String::from_utf8_lossy(&listing)
            .lines()
            .map(|line| {
                let mut ids = line.split(' ').map(str::to_owned);
                Local {
                    id: ids.next().unwrap_or_default(),
                    parent: ids.next(),
                }
            })
            .collect())
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
    /// that does not arrive is sent again on the next tick. It goes as a header on a Git
    /// request, so it reaches the server exactly as syncing does, through the same proxy and
    /// certificates. A remote that is not a server takes no report, so the log says instead
    /// which files the service leaves out, whenever that changes.
    fn report(&mut self, now: SystemTime, outcome: &Outcome, seen: Seen) {
        if !self.config.remote.is_server() {
            self.log_skipped(seen);
            return;
        }
        let mut detail = outcome.detail().to_owned();
        if detail.len() > MAX_DETAIL {
            let cut = (0..=MAX_DETAIL)
                .rev()
                .find(|&at| detail.is_char_boundary(at))
                .unwrap_or(0);
            detail.truncate(cut);
        }
        let total = seen.left.len();
        let mut report = Report {
            state: outcome.state().to_owned(),
            detail,
            head: seen.head,
            skipped: seen
                .left
                .into_iter()
                .take(MAX_SKIPPED)
                .map(|left| Skipped {
                    path: String::from_utf8_lossy(&left.path).into_owned(),
                    reason: left.leave.to_string(),
                })
                .collect(),
            skipped_more: 0,
        };
        report.skipped_more = total - report.skipped.len();
        while report.encode().len() > MAX_REPORT && report.skipped.pop().is_some() {
            report.skipped_more += 1;
        }
        let due = match &self.state.reported {
            None => true,
            Some((last, at)) => *last != report || *at + self.config.report_every <= now,
        };
        if due
            && self
                .git
                .ask_server_with(&format!("{REPORT_HEADER}: {}", report.encode()))
                .is_ok_and(|run| run.success)
        {
            self.state.reported = Some((report, now));
        }
    }

    fn log_skipped(&mut self, seen: Seen) {
        if !seen.looked {
            return;
        }
        let skipped: Vec<Skipped> = seen
            .left
            .into_iter()
            .map(|left| Skipped {
                path: String::from_utf8_lossy(&left.path).into_owned(),
                reason: left.leave.to_string(),
            })
            .collect();
        if skipped != self.state.logged_skipped {
            log::emit("skipped", json!({ "skipped": skipped }));
            self.state.logged_skipped = skipped;
        }
    }
}

/// Why the folder cannot take the server's changes, naming each left-out file in the way.
fn stuck_reason(left: &[&Left]) -> String {
    let mut left = left.to_vec();
    left.sort_by(|a, b| a.path.cmp(&b.path));
    let files: Vec<String> = left
        .iter()
        .map(|left| format!("{} ({})", String::from_utf8_lossy(&left.path), left.leave))
        .collect();
    format!(
        "the server changed files the service leaves out, so the folder cannot take its \
         changes until they are fixed: {}",
        files.join(", ")
    )
}
