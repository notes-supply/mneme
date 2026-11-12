//! The gate for the autosave service: three clones, each with its own service, against one real
//! server, and again against a bare repository with no push check, as any Git host holds one.
//! The test drives every tick with a fake clock, in an order a seeded generator picks, and
//! between ticks does what writers do. `MNEME_SIM_SEEDS` sets how many seeds run,
//! `MNEME_SIM_SEED` runs one seed, and `MNEME_SIM_STEPS` sets the steps per seed.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::fs::{self, File};
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mneme::autosave::{Autosave, Config, Login, Outcome, Remote, Settings, Token};
use mneme::rules::{self, Entry, MAX_FILE, MAX_PAGE};
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::git::{self, Isolation};
use crate::server::{MACHINES, Server};

const QUIET: Duration = Duration::from_secs(15);
const FETCH_EVERY: Duration = Duration::from_secs(30);
const STALE_LOCK: Duration = Duration::from_secs(300);
/// The fake clock starts at 2027-01-15T08:00:00Z.
const START: u64 = 1_800_000_000;
/// Port 9 is discard, which nothing on the test machine listens on.
const UNREACHABLE: &str = "http://127.0.0.1:9/git/wiki.git";
/// A bare remote that is not there, as when its disk or host is gone.
const UNREACHABLE_BARE: &str = "/nonexistent/mneme-simulation/wiki.git";
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const GATE_SEEDS: u64 = 128;
/// The body of a hand commit made to land as a service moves the folder.
const LANDED_AS_IT_MOVED: &str = "An agent committed this as the service moved the folder.";
const GATE_STEPS: usize = 80;

fn env_number<T: std::str::FromStr>(name: &str) -> Option<T> {
    std::env::var(name).ok().map(|value| {
        value
            .parse()
            .unwrap_or_else(|_| panic!("{name} is not a number"))
    })
}

#[test]
fn three_clones_end_equal_to_the_server_with_every_commit_once() {
    simulate(Kind::Server);
}

/// The same against a plain bare repository, which takes any push. The service's own rules must
/// keep every file it leaves out off the remote, since nothing there would refuse one.
#[test]
fn three_clones_end_equal_to_a_bare_remote_with_every_commit_once() {
    simulate(Kind::Bare);
}

/// What the clones sync with.
#[derive(Clone, Copy)]
enum Kind {
    /// A Mneme server, with its push check and the services' reports.
    Server,
    /// A bare repository by its path, with neither.
    Bare,
}

/// A Mneme server, or a bare repository that only stores what it is sent.
enum Upstream {
    Server(Server),
    Bare(TempDir),
}

impl Upstream {
    fn new(kind: Kind, git: &Isolation) -> Self {
        match kind {
            Kind::Server => Self::Server(Server::empty(git)),
            Kind::Bare => {
                let dir = TempDir::new().unwrap();
                let repo = dir.path().join("wiki.git");
                git.ok(
                    dir.path(),
                    &["init", "-q", "--bare", "-b", "main", repo.to_str().unwrap()],
                );
                Self::Bare(dir)
            }
        }
    }

    fn repo(&self) -> PathBuf {
        match self {
            Self::Server(server) => server.repo().to_owned(),
            Self::Bare(dir) => dir.path().join("wiki.git"),
        }
    }

    fn git_url(&self) -> String {
        match self {
            Self::Server(server) => server.git_url(),
            Self::Bare(_) => self.repo().to_str().unwrap().to_owned(),
        }
    }

    fn unreachable(&self) -> &'static str {
        match self {
            Self::Server(_) => UNREACHABLE,
            Self::Bare(_) => UNREACHABLE_BARE,
        }
    }
}

fn simulate(kind: Kind) {
    let steps = env_number("MNEME_SIM_STEPS").unwrap_or(GATE_STEPS);
    let seeds: Vec<u64> = match env_number("MNEME_SIM_SEED") {
        Some(seed) => vec![seed],
        None => (0..env_number("MNEME_SIM_SEEDS").unwrap_or(GATE_SEEDS)).collect(),
    };
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let tally = Mutex::new(Tally::default());
    let workers = thread::available_parallelism().map_or(4, |n| n.get().min(8));
    thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= seeds.len() || failed.load(Ordering::Relaxed) {
                        return;
                    }
                    let seed = seeds[index];
                    let mut sim = Sim::new(seed, kind);
                    let run = panic::catch_unwind(AssertUnwindSafe(|| sim.run(steps)));
                    tally.lock().unwrap().add(&sim.tally);
                    if let Err(cause) = run {
                        failed.store(true, Ordering::Relaxed);
                        eprintln!(
                            "seed {seed} failed. Rerun it with MNEME_SIM_SEED={seed}.\n{}",
                            sim.trace.join("\n")
                        );
                        panic::resume_unwind(cause);
                    }
                }
            });
        }
    });
    eprintln!(
        "{} seeds of {steps} steps passed: {:?}",
        seeds.len(),
        tally.into_inner().unwrap()
    );
}

/// What the seeds exercised, so a pass can be told from a run that did nothing.
#[derive(Debug, Default)]
struct Tally {
    ticks: usize,
    saved: usize,
    pulled: usize,
    pushed: usize,
    offline: usize,
    /// Same line, same new page, edit against delete, delete against edit, rename against rename.
    overlaps_on_purpose: [usize; 5],
    hand_commits: usize,
    hand_merges: usize,
    dead_locks: usize,
    hand_commits_as_the_folder_moved: usize,
    bad_files: usize,
    bad_files_fixed: usize,
    commits_made: usize,
    replays_that_overlapped: usize,
    replays_that_merged: usize,
    dropped_as_already_there: usize,
}

impl Tally {
    fn add(&mut self, other: &Self) {
        self.ticks += other.ticks;
        self.saved += other.saved;
        self.pulled += other.pulled;
        self.pushed += other.pushed;
        self.offline += other.offline;
        for (total, more) in self
            .overlaps_on_purpose
            .iter_mut()
            .zip(other.overlaps_on_purpose)
        {
            *total += more;
        }
        self.hand_commits += other.hand_commits;
        self.hand_merges += other.hand_merges;
        self.dead_locks += other.dead_locks;
        self.hand_commits_as_the_folder_moved += other.hand_commits_as_the_folder_moved;
        self.bad_files += other.bad_files;
        self.bad_files_fixed += other.bad_files_fixed;
        self.commits_made += other.commits_made;
        self.replays_that_overlapped += other.replays_that_overlapped;
        self.replays_that_merged += other.replays_that_merged;
        self.dropped_as_already_there += other.dropped_as_already_there;
    }
}

struct Clone {
    machine: &'static str,
    token: &'static str,
    dir: TempDir,
    git: Isolation,
    service: Autosave,
    online: bool,
    /// Lines of the reflog of `main` already read.
    reflog: usize,
    /// Files the service must leave in the folder, as its report names them, with the reason.
    bad: BTreeMap<String, &'static str>,
}

impl Clone {
    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn git_at(&self, now: SystemTime, args: &[&str]) -> Output {
        let date = format!("@{} +0000", secs(now));
        self.git
            .apply(Command::new("git").current_dir(self.path()).args(args))
            .env("GIT_AUTHOR_NAME", format!("Agent on {}", self.machine))
            .env("GIT_AUTHOR_EMAIL", "agent@example.invalid")
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        self.git.ok(self.path(), args)
    }

    fn header(&self) -> String {
        format!("http.extraHeader=Authorization: Bearer {}", self.token)
    }

    fn head(&self) -> String {
        self.ok(&["rev-parse", "HEAD"]).trim().to_owned()
    }

    /// The changes in the folder, less the bad files the service leaves there.
    fn status(&self) -> String {
        self.ok(&["status", "--porcelain", "--untracked-files=all"])
            .lines()
            .filter(|line| !self.bad.contains_key(&line[3..]))
            .flat_map(|line| [line, "\n"])
            .collect()
    }

    /// The pages in the folder, less the bad ones.
    fn pages(&self) -> Vec<String> {
        let mut pages = Vec::new();
        walk(self.path(), "", &mut pages);
        pages.retain(|page| !self.bad.contains_key(page));
        pages.sort();
        pages
    }
}

fn walk(dir: &Path, prefix: &str, pages: &mut Vec<String>) {
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().into_string().unwrap();
        if name == ".git" {
            continue;
        }
        let path = format!("{prefix}{name}");
        if entry.file_type().unwrap().is_dir() {
            walk(&entry.path(), &format!("{path}/"), pages);
        } else if is_page(&name) {
            pages.push(path);
        }
    }
}

fn is_page(path: &str) -> bool {
    Path::new(path)
        .extension()
        .is_some_and(|extension| extension == "md")
}

fn secs(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH).unwrap().as_secs()
}

/// A commit as the oracle compares it.
#[derive(Clone, Debug)]
struct Commit {
    id: String,
    author: String,
    email: String,
    time: String,
    parents: Vec<String>,
    tree: String,
    message: String,
}

impl Commit {
    fn read(git: &Isolation, repo: &Path, id: &str) -> Self {
        Self::read_all(git, repo, &["--no-walk", id]).remove(0)
    }

    /// The commits `git log` lists for `args`, with one command.
    fn read_all(git: &Isolation, repo: &Path, args: &[&str]) -> Vec<Self> {
        let format = "--format=%H%x01%an%x01%ae%x01%at%x01%P%x01%T%x01%B";
        let text = git.ok(repo, &[&["log", "-z", format], args].concat());
        text.split('\0')
            .filter(|record| !record.is_empty())
            .map(|record| {
                let fields: Vec<&str> = record.splitn(7, '\x01').collect();
                Self {
                    id: fields[0].to_owned(),
                    author: fields[1].to_owned(),
                    email: fields[2].to_owned(),
                    time: fields[3].to_owned(),
                    parents: fields[4].split_whitespace().map(str::to_owned).collect(),
                    tree: fields[5].to_owned(),
                    message: fields[6].trim_end().to_owned(),
                }
            })
            .collect()
    }

    fn subject(&self) -> &str {
        self.message.lines().next().unwrap_or_default()
    }

    /// Author, author time and subject: what stays the same when a commit is replayed.
    fn key(&self) -> (String, String, String) {
        (
            self.author.clone(),
            self.time.clone(),
            self.subject().to_owned(),
        )
    }

    fn overlaps(&self) -> usize {
        self.message
            .lines()
            .filter(|line| line.starts_with("Overlap: "))
            .count()
    }
}

struct Sim {
    seed: u64,
    rng: fastrand::Rng,
    server: Upstream,
    git: Isolation,
    /// The Git program the service of one clone runs, which can make a hand commit land as it
    /// moves the folder. Running it costs a shell for every Git command, so only that clone has
    /// it, a different one from seed to seed.
    racy: (TempDir, std::path::PathBuf),
    racing: usize,
    clones: Vec<Clone>,
    now: SystemTime,
    counter: u64,
    /// Every commit a clone made, by clone, from its reflog.
    made: Vec<(usize, String)>,
    outcomes: Vec<(usize, Outcome)>,
    trace: Vec<String>,
    /// The server's head once the starting pages are in.
    base: String,
    tally: Tally,
}

impl Sim {
    fn new(seed: u64, kind: Kind) -> Self {
        let git = Isolation::new();
        let server = Upstream::new(kind, &git);
        let now = UNIX_EPOCH + Duration::from_secs(START);
        let racy = TempDir::new().unwrap();
        let program = git::racy_git(racy.path());
        let mut sim = Self {
            seed,
            rng: fastrand::Rng::with_seed(seed),
            server,
            git,
            racy: (racy, program),
            racing: usize::try_from(seed % 3).unwrap(),
            clones: Vec::new(),
            now,
            counter: 0,
            made: Vec::new(),
            outcomes: Vec::new(),
            trace: Vec::new(),
            base: String::new(),
            tally: Tally::default(),
        };
        sim.start_pages();
        for (index, (machine, token)) in MACHINES.into_iter().enumerate() {
            let clone = sim.clone_as(machine, token, index == sim.racing);
            sim.clones.push(clone);
        }
        sim
    }

    fn start_pages(&mut self) {
        let dir = TempDir::new().unwrap();
        let header = format!("http.extraHeader=Authorization: Bearer {}", MACHINES[0].1);
        let url = self.server.git_url();
        self.git
            .ok(dir.path(), &["-c", &header, "clone", "-q", &url, "."]);
        for page in 0..12 {
            let mut text = format!("# Page {page}\n\n");
            for line in 0..8 {
                let _ = writeln!(text, "start {page}.{line}");
            }
            let path = if page % 4 == 0 {
                format!("notes/start-{page}.md")
            } else {
                format!("start-{page}.md")
            };
            let file = dir.path().join(&path);
            fs::create_dir_all(file.parent().unwrap()).unwrap();
            fs::write(file, text).unwrap();
        }
        self.git.ok(dir.path(), &["add", "-A"]);
        self.git
            .ok(dir.path(), &["commit", "-q", "-m", "Start the wiki"]);
        self.git
            .ok(dir.path(), &["-c", &header, "push", "-q", "origin", "main"]);
        self.base = self
            .git
            .ok(dir.path(), &["rev-parse", "HEAD"])
            .trim()
            .to_owned();
    }

    fn clone_as(&self, machine: &'static str, token: &'static str, racing: bool) -> Clone {
        let dir = TempDir::new().unwrap();
        let git = Isolation::new();
        let header = format!("http.extraHeader=Authorization: Bearer {token}");
        let url = self.server.git_url();
        git.ok(dir.path(), &["-c", &header, "clone", "-q", &url, "."]);
        git.ok(dir.path(), &["config", "gc.auto", "0"]);
        let program = if racing {
            self.racy.1.clone()
        } else {
            "git".into()
        };
        let service = match &self.server {
            Upstream::Server(server) => Autosave::new(Config {
                folder: dir.path().to_owned(),
                server: server.url.clone(),
                token: Token(token.to_owned()),
                machine: machine.to_owned(),
                quiet: QUIET,
                fetch_every: FETCH_EVERY,
                report_every: Duration::from_secs(300),
                stale_lock: STALE_LOCK,
                git: program,
            }),
            Upstream::Bare(_) => Autosave::new(Settings {
                folder: dir.path().to_owned(),
                remote: Remote::git(&url, Login::default()).unwrap(),
                machine: machine.to_owned(),
                quiet: QUIET,
                fetch_every: FETCH_EVERY,
                report_every: Duration::from_secs(300),
                stale_lock: STALE_LOCK,
                git: program,
            }),
        };
        let mut clone = Clone {
            machine,
            token,
            dir,
            git,
            service,
            online: true,
            reflog: 0,
            bad: BTreeMap::new(),
        };
        clone.reflog = reflog(&clone).len();
        clone
    }

    fn run(&mut self, steps: usize) {
        for _ in 0..steps {
            self.step();
        }
        self.converge();
        self.check();
        let (overlapped, merged, dropped) = self.check_every_commit_once();
        self.tally.commits_made += self.made.len();
        self.tally.replays_that_overlapped += overlapped;
        self.tally.replays_that_merged += merged;
        self.tally.dropped_as_already_there += dropped;
    }

    fn token(&mut self, clone: usize) -> String {
        self.counter += 1;
        format!("{}-{}", self.clones[clone].machine, self.counter)
    }

    fn note(&mut self, clone: usize, what: &str) {
        let line = format!(
            "{:>6} {:<8} {what}",
            secs(self.now) - START,
            self.clones[clone].machine
        );
        self.trace.push(line);
    }

    fn step(&mut self) {
        let clone = self.rng.usize(..self.clones.len());
        match self.rng.u32(..100) {
            0..38 => self.tick(clone),
            38..40 => self.hand_commit_as_the_folder_moves(),
            40..50 => self.create(clone),
            50..62 => self.edit(clone),
            62..67 => self.append(clone),
            67..71 => self.delete(clone),
            71..75 => self.rename(clone, false),
            75..79 => self.rename(clone, true),
            79..84 => self.overlap_on_purpose(clone),
            84..86 => self.delete_on_two(clone),
            86..89 => self.hand_commit(clone),
            89..92 => self.hand_pull(clone),
            92..94 => self.dead_lock(clone),
            94..97 => self.bad_file(clone),
            _ => self.toggle_network(clone),
        }
    }

    fn advance(&mut self, by: Duration) {
        self.now += by;
    }

    fn tick(&mut self, clone: usize) {
        let by = Duration::from_secs(self.rng.u64(1..=12));
        self.advance(by);
        self.tick_now(clone);
    }

    fn tick_now(&mut self, clone: usize) -> Outcome {
        let now = self.now;
        let outcome = self.clones[clone].service.tick(now);
        self.note(clone, &format!("tick: {outcome:?}"));
        self.tally.ticks += 1;
        match outcome {
            Outcome::Saved => self.tally.saved += 1,
            Outcome::Pulled => self.tally.pulled += 1,
            Outcome::Pushed => self.tally.pushed += 1,
            Outcome::Offline(_) => self.tally.offline += 1,
            _ => {}
        }
        self.outcomes.push((clone, outcome.clone()));
        let ran = self.clones[clone].path().join(".git/mneme-test-race.ran");
        if ran.exists() {
            fs::remove_file(ran).unwrap();
            self.note(clone, "the hand commit landed as the folder moved");
            self.tally.hand_commits_as_the_folder_moved += 1;
        }
        self.read_reflog(clone);
        outcome
    }

    /// Records every commit the clone made since the last look, from the reflog of `main`.
    fn read_reflog(&mut self, clone: usize) {
        let lines = reflog(&self.clones[clone]);
        for line in &lines[self.clones[clone].reflog..] {
            let (refs, message) = line.split_once('\t').unwrap();
            if message.starts_with("commit") {
                let new = refs.split(' ').nth(1).unwrap().to_owned();
                self.made.push((clone, new));
            }
        }
        self.clones[clone].reflog = lines.len();
    }

    fn write(&self, clone: usize, path: &str, text: &str) {
        let file = self.clones[clone].path().join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, text).unwrap();
        File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(self.now)
            .unwrap();
    }

    fn read(&self, clone: usize, path: &str) -> String {
        fs::read_to_string(self.clones[clone].path().join(path)).unwrap()
    }

    fn pick_page(&mut self, clone: usize) -> Option<String> {
        let pages = self.clones[clone].pages();
        (!pages.is_empty()).then(|| pages[self.rng.usize(..pages.len())].clone())
    }

    fn create(&mut self, clone: usize) {
        let token = self.token(clone);
        let path = if self.rng.bool() {
            format!("page-{token}.md")
        } else {
            format!("notes/page-{token}.md")
        };
        let mut text = format!("# Page {token}\n\n");
        for line in 0..self.rng.usize(2..8) {
            let _ = writeln!(text, "text {token}.{line}");
        }
        self.write(clone, &path, &text);
        self.note(clone, &format!("create {path}"));
    }

    /// Replaces one line of `path` and returns the new text.
    fn replace_line(&mut self, clone: usize, path: &str, line: Option<usize>) -> String {
        let token = self.token(clone);
        let mut lines: Vec<String> = self.read(clone, path).lines().map(str::to_owned).collect();
        if lines.is_empty() {
            lines.push(String::new());
        }
        let at = line
            .unwrap_or_else(|| self.rng.usize(..lines.len()))
            .min(lines.len() - 1);
        lines[at] = format!("edit {token}");
        let text = lines.join("\n") + "\n";
        self.write(clone, path, &text);
        self.note(clone, &format!("edit {path} line {at}"));
        text
    }

    fn edit(&mut self, clone: usize) {
        if let Some(path) = self.pick_page(clone) {
            self.replace_line(clone, &path, None);
        }
    }

    fn append(&mut self, clone: usize) {
        if let Some(path) = self.pick_page(clone) {
            let token = self.token(clone);
            let text = self.read(clone, &path) + &format!("append {token}\n");
            self.write(clone, &path, &text);
            self.note(clone, &format!("append {path}"));
        }
    }

    /// Deletes a page, but never so many in one save that the service would stop.
    fn delete(&mut self, clone: usize) {
        let deleted = self.clones[clone]
            .status()
            .lines()
            .filter(|line| line[..2].contains('D') && is_page(line))
            .count();
        if deleted >= 4 || self.clones[clone].pages().len() < 6 {
            return;
        }
        if let Some(path) = self.pick_page(clone) {
            fs::remove_file(self.clones[clone].path().join(&path)).unwrap();
            self.note(clone, &format!("delete {path}"));
        }
    }

    fn rename(&mut self, clone: usize, edit: bool) {
        let Some(from) = self.pick_page(clone) else {
            return;
        };
        let token = self.token(clone);
        let to = if self.rng.bool() {
            format!("moved-{token}.md")
        } else {
            format!("archive/moved-{token}.md")
        };
        let target = self.clones[clone].path().join(&to);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        fs::rename(self.clones[clone].path().join(&from), target).unwrap();
        self.note(clone, &format!("rename {from} to {to}"));
        if edit {
            self.replace_line(clone, &to, None);
        }
    }

    /// Two clones that both have a page as the server has it change it in ways that cannot
    /// both stand, and both save before either syncs. The second to integrate keeps its own
    /// side, says so, and pushes.
    fn overlap_on_purpose(&mut self, first: usize) {
        let second = self.other(first);
        if !self.clones[first].online || !self.clones[second].online {
            return;
        }
        let kind = self.rng.usize(..5);
        let path = if kind == 1 {
            let token = self.token(first);
            format!("shared-{token}.md")
        } else {
            match self.shared_page(first, second, true) {
                Some(path) => path,
                None => return,
            }
        };
        let kept = match kind {
            0 => {
                let line = self
                    .rng
                    .usize(..self.read(first, &path).lines().count().max(1));
                self.replace_line(first, &path, Some(line));
                Some((path.clone(), self.replace_line(second, &path, Some(line))))
            }
            1 => {
                for clone in [first, second] {
                    let token = self.token(clone);
                    self.write(clone, &path, &format!("# Shared\n\nwritten by {token}\n"));
                }
                self.note(second, &format!("both create {path}"));
                Some((path.clone(), self.read(second, &path)))
            }
            2 => {
                self.replace_line(first, &path, None);
                fs::remove_file(self.clones[second].path().join(&path)).unwrap();
                self.note(
                    second,
                    &format!("delete {path}, which the other clone edits"),
                );
                None
            }
            3 => {
                fs::remove_file(self.clones[first].path().join(&path)).unwrap();
                self.note(
                    first,
                    &format!("delete {path}, which the other clone edits"),
                );
                Some((path.clone(), self.replace_line(second, &path, None)))
            }
            _ => {
                let mut targets = Vec::new();
                for clone in [first, second] {
                    let token = self.token(clone);
                    let to = format!("moved-{token}.md");
                    fs::rename(
                        self.clones[clone].path().join(&path),
                        self.clones[clone].path().join(&to),
                    )
                    .unwrap();
                    self.note(
                        clone,
                        &format!("rename {path} to {to}, as the other clone does"),
                    );
                    targets.push(to);
                }
                let to = targets.pop().unwrap();
                Some((to.clone(), self.read(second, &to)))
            }
        };
        self.advance(QUIET + Duration::from_secs(1));
        self.tally.overlaps_on_purpose[kind] += 1;
        self.tick_now(first);
        let outcome = self.tick_now(second);
        let seed = self.seed;
        assert_eq!(
            outcome,
            Outcome::Pushed,
            "seed {seed}: the second clone pushes its side"
        );
        let (head, commit) = self.saved_head();
        assert_eq!(
            commit.overlaps(),
            1,
            "seed {seed}: {head} has one Overlap trailer: {}",
            commit.message
        );
        match kept {
            Some((path, text)) => assert_eq!(
                self.server_text(&head, &path).as_deref(),
                Some(text.as_bytes()),
                "seed {seed}: {path} has the text the second clone saved"
            ),
            None => assert_eq!(
                self.server_text(&head, &path),
                None,
                "seed {seed}: {path} stays deleted, as the second clone saved it"
            ),
        }
    }

    /// Two clones delete the same page before either syncs, so the second deletion finds the
    /// page already gone.
    fn delete_on_two(&mut self, first: usize) {
        let second = self.other(first);
        let roomy = |sim: &Self, clone: usize| {
            let target = &sim.clones[clone];
            let deleted = target
                .status()
                .lines()
                .filter(|line| line[..2].contains('D') && is_page(line))
                .count();
            deleted < 4 && target.pages().len() >= 6
        };
        if !roomy(self, first) || !roomy(self, second) {
            return;
        }
        let Some(path) = self.shared_page(first, second, false) else {
            return;
        };
        for clone in [first, second] {
            fs::remove_file(self.clones[clone].path().join(&path)).unwrap();
            self.note(clone, &format!("delete {path}, on another clone too"));
        }
    }

    fn other(&mut self, clone: usize) -> usize {
        (clone + 1 + self.rng.usize(..self.clones.len() - 1)) % self.clones.len()
    }

    /// A page with the same text on both clones, and on the server's main when `on_server`.
    fn shared_page(&mut self, first: usize, second: usize, on_server: bool) -> Option<String> {
        let server_head = self.server_head();
        let candidates: Vec<String> = self.clones[first]
            .pages()
            .into_iter()
            .filter(|path| {
                let text = fs::read(self.clones[first].path().join(path)).unwrap();
                fs::read(self.clones[second].path().join(path))
                    .ok()
                    .as_ref()
                    == Some(&text)
                    && (!on_server
                        || self.server_text(&server_head, path).as_deref() == Some(&text[..]))
            })
            .collect();
        (!candidates.is_empty()).then(|| candidates[self.rng.usize(..candidates.len())].clone())
    }

    fn hand_commit(&mut self, clone: usize) {
        if self.clones[clone].status().is_empty() {
            return;
        }
        let token = self.token(clone);
        let message = format!("Hand edit {token}\n\nAn agent committed this by hand.");
        let now = self.now;
        let target = &self.clones[clone];
        let mut add = vec![
            "add".to_owned(),
            "-A".to_owned(),
            "--".to_owned(),
            ".".to_owned(),
        ];
        for path in target.bad.keys() {
            let top = path.split('/').next().unwrap();
            add.push(format!(":(exclude){top}"));
        }
        let add: Vec<&str> = add.iter().map(String::as_str).collect();
        target.ok(&add);
        let output = target.git_at(now, &["commit", "-q", "-m", &message]);
        assert!(output.status.success(), "{output:?}");
        self.note(clone, &format!("hand commit {token}"));
        self.tally.hand_commits += 1;
        self.read_reflog(clone);
    }

    /// Makes an agent's hand commit of a new page land in the middle of the racing clone's next
    /// move of the folder, right before it moves the folder or right before it moves `main`.
    fn hand_commit_as_the_folder_moves(&mut self) {
        let clone = self.racing;
        let race = self.clones[clone].path().join(".git/mneme-test-race");
        if race.exists() {
            return;
        }
        let token = self.token(clone);
        let before = if self.rng.bool() {
            "read-tree -m"
        } else {
            "update-ref"
        };
        let machine = self.clones[clone].machine;
        let script = format!(
            "export GIT_AUTHOR_NAME='Agent on {machine}' GIT_AUTHOR_EMAIL=agent@example.invalid \\
             GIT_AUTHOR_DATE='@{} +0000'\n\
             printf '# Page {token}\\n\\ntext {token}\\n' > \"$GIT_WORK_TREE/race-{token}.md\"\n\
             git add -- race-{token}.md && git commit -q -m 'Hand edit {token}' \\
             -m '{LANDED_AS_IT_MOVED}'\n",
            secs(self.now)
        );
        fs::write(&race, script).unwrap();
        fs::write(race.with_extension("before"), before).unwrap();
        self.note(clone, &format!("a hand commit waits for the next {before}"));
    }

    /// `git pull` with a merge, as an agent might run it after committing its work. A pull
    /// that stops on a conflict is undone, as the agent would before walking away.
    fn hand_pull(&mut self, clone: usize) {
        if !self.clones[clone].online {
            return;
        }
        self.hand_commit(clone);
        let now = self.now;
        let target = &self.clones[clone];
        let header = target.header();
        let output = target.git_at(
            now,
            &[
                "-c",
                &header,
                "pull",
                "-q",
                "--no-rebase",
                "--no-edit",
                "origin",
                "main",
            ],
        );
        let merging = target.path().join(".git/MERGE_HEAD").exists();
        let merged = Commit::read(&target.git, target.path(), "HEAD")
            .parents
            .len()
            > 1;
        if merging {
            target.ok(&["merge", "--abort"]);
        }
        let what = format!(
            "hand pull: {}{}",
            if output.status.success() {
                "done"
            } else {
                "refused"
            },
            if merging { ", conflict undone" } else { "" }
        );
        self.note(clone, &what);
        self.tally.hand_merges += usize::from(merged && !merging);
        self.read_reflog(clone);
    }

    /// A Git command that died a while ago left its `index.lock`, and half the time half an
    /// index. The clone's next tick must clear both and carry on.
    fn dead_lock(&mut self, clone: usize) {
        let git_dir = self.clones[clone].path().join(".git");
        let lock = git_dir.join("index.lock");
        fs::write(&lock, "").unwrap();
        let age = STALE_LOCK + Duration::from_secs(self.rng.u64(1..600));
        File::options()
            .write(true)
            .open(&lock)
            .unwrap()
            .set_modified(self.now - age)
            .unwrap();
        let broken = self.rng.bool();
        if broken {
            fs::write(git_dir.join("index"), "half an index").unwrap();
        }
        self.note(
            clone,
            &format!("a dead command's lock, broken index {broken}"),
        );
        self.tally.dead_locks += 1;
        self.tick(clone);
        assert!(!lock.exists(), "seed {}: the dead lock is gone", self.seed);
    }

    /// A writer drops a file the server would refuse or Git cannot store, each at a new top-level
    /// name, or fixes one: a bad page becomes a good one, and any other bad file goes.
    fn bad_file(&mut self, clone: usize) {
        let fixable: Vec<String> = self.clones[clone].bad.keys().cloned().collect();
        if !fixable.is_empty() && self.rng.u32(..3) == 0 {
            let path = fixable[self.rng.usize(..fixable.len())].clone();
            let reason = self.clones[clone].bad.remove(&path).unwrap();
            let top = path.split('/').next().unwrap();
            let file = self.clones[clone].path().join(top);
            if file.is_dir() && !file.is_symlink() {
                fs::remove_dir_all(&file).unwrap();
            } else {
                fs::remove_file(&file).unwrap();
            }
            if matches!(reason, "a symlink" | "a page over 1 MiB") {
                let token = self.token(clone);
                self.write(clone, &path, &format!("# Fixed {token}\n\ntext {token}\n"));
            }
            self.note(clone, &format!("fix {path}"));
            self.tally.bad_files_fixed += 1;
            return;
        }
        let token = self.token(clone);
        let folder = self.clones[clone].path().to_owned();
        let sparse = |path: &Path, size: u64| {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            File::create(path).unwrap().set_len(size).unwrap();
        };
        let (path, reason) = match self.rng.usize(..6) {
            0 => {
                let path = format!("link-{token}.md");
                let target = self
                    .pick_page(clone)
                    .unwrap_or_else(|| "nowhere.md".to_owned());
                std::os::unix::fs::symlink(target, folder.join(&path)).unwrap();
                (path, "a symlink")
            }
            1 => {
                let path = format!("big-{token}.md");
                sparse(&folder.join(&path), MAX_PAGE + 1);
                (path, "a page over 1 MiB")
            }
            2 => {
                let path = format!("huge-{token}.bin");
                sparse(&folder.join(&path), MAX_FILE + 1);
                (path, "a file over 10 MiB")
            }
            3 => {
                let path = format!("nested-{token}/");
                fs::create_dir(folder.join(&path)).unwrap();
                self.clones[clone].ok(&["init", "-q", path.trim_end_matches('/')]);
                (path, "a submodule or nested repository")
            }
            4 => {
                let path = format!("dotgit-{token}/.GIT/config");
                self.write(clone, &path, "[core]\n");
                (path, "a path named .git")
            }
            _ => {
                let path = format!("odd-{token}/git~1/notes.md");
                self.write(clone, &path, &format!("# Notes {token}\n"));
                (path, "a name Git will not store")
            }
        };
        self.note(clone, &format!("bad file {path}: {reason}"));
        self.clones[clone].bad.insert(path, reason);
        self.tally.bad_files += 1;
    }

    fn toggle_network(&mut self, clone: usize) {
        let online = !self.clones[clone].online;
        let url = if online {
            self.server.git_url()
        } else {
            self.server.unreachable().to_owned()
        };
        self.clones[clone].ok(&["config", "remote.origin.url", &url]);
        self.clones[clone].online = online;
        self.note(clone, if online { "online" } else { "offline" });
    }

    /// The server's head below the hand commits that landed as a folder moved, which a tick
    /// replays on top of what it saved.
    fn saved_head(&self) -> (String, Commit) {
        let mut head = self.server_head();
        let mut commit = Commit::read(&self.git, &self.server.repo(), &head);
        while commit.message.contains(LANDED_AS_IT_MOVED) {
            head.clone_from(&commit.parents[0]);
            commit = Commit::read(&self.git, &self.server.repo(), &head);
        }
        (head, commit)
    }

    fn server_head(&self) -> String {
        self.git
            .ok(&self.server.repo(), &["rev-parse", "main"])
            .trim()
            .to_owned()
    }

    fn server_text(&self, commit: &str, path: &str) -> Option<Vec<u8>> {
        let output = self.git.git(
            &self.server.repo(),
            &["cat-file", "blob", &format!("{commit}:{path}")],
        );
        output.status.success().then_some(output.stdout)
    }

    /// Every clone goes online and ticks, a fetch interval apart, until a round changes
    /// nothing and every clone holds the server's head.
    fn converge(&mut self) {
        for clone in 0..self.clones.len() {
            if !self.clones[clone].online {
                self.toggle_network(clone);
            }
        }
        self.trace.push("--- converge".to_owned());
        for _ in 0..12 {
            let mut changed = false;
            for clone in 0..self.clones.len() {
                self.advance(FETCH_EVERY + Duration::from_secs(1));
                changed |= self.tick_now(clone) != Outcome::Idle;
            }
            let head = self.server_head();
            let settled = self
                .clones
                .iter()
                .all(|clone| clone.head() == head && clone.status().is_empty());
            if !changed && settled {
                return;
            }
        }
    }

    fn check(&self) {
        let seed = self.seed;
        let head = self.server_head();
        for clone in &self.clones {
            assert_eq!(
                clone.head(),
                head,
                "seed {seed}: {} has the server's main",
                clone.machine
            );
            assert_eq!(
                clone.status(),
                "",
                "seed {seed}: {} is clean",
                clone.machine
            );
        }
        let merges = self.git.ok(
            &self.server.repo(),
            &["rev-list", "--min-parents=2", "main"],
        );
        assert_eq!(merges, "", "seed {seed}: the server's history is one line");
        match &self.server {
            Upstream::Server(server) => self.check_reports(server),
            Upstream::Bare(_) => self.check_rules_kept(),
        }
        let stuck: Vec<_> = self
            .outcomes
            .iter()
            .filter(|(_, outcome)| matches!(outcome, Outcome::Blocked(_) | Outcome::Stopped(_)))
            .collect();
        assert!(
            stuck.is_empty(),
            "seed {seed}: no tick was blocked or stopped: {stuck:?}"
        );
    }

    /// Each clone's report lists the files its service leaves in the folder.
    fn check_reports(&self, server: &Server) {
        let seed = self.seed;
        let machines = server.json("/v1/machines");
        for clone in &self.clones {
            let reported = machines["machines"]
                .as_array()
                .unwrap()
                .iter()
                .find(|machine| machine["name"] == clone.machine)
                .unwrap();
            let bad: Vec<Value> = clone
                .bad
                .iter()
                .map(|(path, reason)| json!({ "path": path, "reason": reason }))
                .collect();
            assert_eq!(
                reported["skipped"],
                Value::Array(bad),
                "seed {seed}: {} reports the files it leaves in the folder",
                clone.machine
            );
        }
    }

    /// Every commit on a remote with no push check keeps the rules the server's check would
    /// hold it to, so a file the services leave out never got there.
    fn check_rules_kept(&self) {
        let seed = self.seed;
        let repo = self.server.repo();
        let range = format!("{}..main", self.base);
        for commit in self.git.ok(&repo, &["rev-list", &range]).lines() {
            let listing = self
                .git
                .ok(&repo, &["ls-tree", "-r", "-t", "-z", "-l", commit]);
            for record in listing.split('\0').filter(|record| !record.is_empty()) {
                let (facts, path) = record.split_once('\t').unwrap();
                let facts: Vec<&str> = facts.split_whitespace().collect();
                let entry = match (facts[0], facts[1]) {
                    ("120000", _) => Entry::Symlink,
                    ("160000", _) => Entry::Repository,
                    (_, "tree") => Entry::Folder,
                    _ => Entry::File(facts[3].parse().unwrap()),
                };
                assert_eq!(
                    rules::check(path.as_bytes(), entry),
                    None,
                    "seed {seed}: commit {commit} on the bare remote holds {path}"
                );
            }
        }
    }

    /// Returns how many replays overlapped, how many merged cleanly, and how many commits were
    /// left out because their change was already there.
    fn check_every_commit_once(&self) -> (usize, usize, usize) {
        let seed = self.seed;
        let repo = &self.server.repo();
        let range = format!("{}..main", self.base);
        let on_server = Commit::read_all(&self.git, repo, &["--reverse", &range]);
        let base_tree = self
            .git
            .ok(repo, &["rev-parse", &format!("{}^{{tree}}", self.base)]);
        let mut trees: HashMap<&str, &str> = HashMap::new();
        trees.insert(&self.base, base_tree.trim());
        for commit in &on_server {
            trees.insert(&commit.id, &commit.tree);
        }
        for commit in &on_server {
            let parent = trees.get(commit.parents[0].as_str()).copied();
            assert_ne!(
                parent.unwrap_or_default(),
                commit.tree,
                "seed {seed}: {} on main is not empty",
                commit.id
            );
        }
        let mut by_key: HashMap<_, Vec<&Commit>> = HashMap::new();
        for commit in &on_server {
            by_key.entry(commit.key()).or_default().push(commit);
        }
        let mut made: Vec<(usize, Commit)> = Vec::new();
        for (index, clone) in self.clones.iter().enumerate() {
            let ids: Vec<&str> = self
                .made
                .iter()
                .filter(|(by, _)| *by == index)
                .map(|(_, id)| id.as_str())
                .collect();
            if ids.is_empty() {
                continue;
            }
            let args = [&["--no-walk=unsorted"][..], &ids].concat();
            made.extend(
                Commit::read_all(&clone.git, clone.path(), &args)
                    .into_iter()
                    .filter(|commit| commit.parents.len() <= 1)
                    .map(|commit| (index, commit)),
            );
        }
        let mut made_by_key: BTreeMap<_, (usize, &Commit)> = BTreeMap::new();
        for (clone, commit) in &made {
            let earlier = made_by_key.insert(commit.key(), (*clone, commit));
            assert!(
                earlier.is_none(),
                "seed {seed}: two made commits share {:?}",
                commit.key()
            );
        }
        for (key, commits) in &by_key {
            assert_eq!(commits.len(), 1, "seed {seed}: {key:?} is on main once");
            assert!(
                made_by_key.contains_key(key),
                "seed {seed}: {key:?} on main was made by a clone"
            );
        }
        let (mut overlapped, mut merged, mut dropped) = (0, 0, 0);
        for (clone, commit) in &made {
            let Some(found) = by_key.get(&commit.key()).map(|found| found[0]) else {
                self.check_already_there(*clone, commit);
                dropped += 1;
                continue;
            };
            assert_eq!(found.email, commit.email, "seed {seed}: the author is kept");
            assert!(
                found.message.starts_with(&commit.message),
                "seed {seed}: the message of {} is kept in {}",
                commit.id,
                found.id
            );
            if self.check_overlap(*clone, commit, found) {
                overlapped += 1;
            } else if found.id != commit.id {
                merged += 1;
            }
        }
        (overlapped, merged, dropped)
    }

    /// A made commit missing from main is fine only when all it did was delete pages that
    /// main deletes too, so that replaying it changed nothing.
    fn check_already_there(&self, clone: usize, commit: &Commit) {
        let seed = self.seed;
        let target = &self.clones[clone];
        let parent = commit.parents.first().map_or(EMPTY_TREE, String::as_str);
        let changes = target.ok(&[
            "diff-tree",
            "-r",
            "-z",
            "--name-status",
            "--no-renames",
            parent,
            &commit.id,
        ]);
        let fields: Vec<&str> = changes
            .split('\0')
            .filter(|field| !field.is_empty())
            .collect();
        for change in fields.chunks(2) {
            assert_eq!(
                change[0], "D",
                "seed {seed}: {} by {} is missing from main, yet it changed {}",
                commit.id, target.machine, change[1]
            );
            let range = format!("{}..main", self.base);
            let deletions = self.git.ok(
                &self.server.repo(),
                &[
                    "log",
                    "--diff-filter=D",
                    "--format=%H",
                    &range,
                    "--",
                    change[1],
                ],
            );
            assert!(
                !deletions.is_empty(),
                "seed {seed}: {} deleted {}, which main never deletes",
                commit.id,
                change[1]
            );
        }
    }

    /// Replays the clone's commit onto the commit below it on main with Git's own merge. When
    /// that overlaps, main's commit says so and has the clone's text of every overlapping path.
    /// When it does not, main's commit is exactly that merge. Returns whether it overlapped.
    fn check_overlap(&self, clone: usize, made: &Commit, found: &Commit) -> bool {
        let seed = self.seed;
        let target = &self.clones[clone];
        let base = made.parents.first().map_or(EMPTY_TREE, String::as_str);
        let below = &found.parents[0];
        let output = target.git.git(
            target.path(),
            &[
                "merge-tree",
                "--write-tree",
                "-z",
                "--no-messages",
                &format!("--merge-base={base}"),
                below,
                &made.id,
            ],
        );
        let text = String::from_utf8(output.stdout).unwrap();
        let mut records = text.split('\0');
        let merged = records.next().unwrap().to_owned();
        let tree = &found.tree;
        match output.status.code() {
            Some(0) => {
                assert_eq!(
                    found.overlaps(),
                    made.overlaps(),
                    "seed {seed}: {} did not overlap and has no Overlap trailer",
                    found.id
                );
                assert_eq!(tree, &merged, "seed {seed}: {} is Git's merge", found.id);
                false
            }
            Some(1) => {
                assert_eq!(
                    found.overlaps(),
                    made.overlaps() + 1,
                    "seed {seed}: {} overlapped and has an Overlap trailer",
                    found.id
                );
                let mut theirs: BTreeMap<&str, Option<&str>> = BTreeMap::new();
                for record in records.take_while(|record| !record.is_empty()) {
                    let (facts, path) = record.split_once('\t').unwrap();
                    let facts: Vec<&str> = facts.split(' ').collect();
                    let entry = theirs.entry(path).or_insert(None);
                    if facts[2] == "3" {
                        *entry = Some(facts[1]);
                    }
                }
                for (path, blob) in theirs {
                    let output = target.git.git(
                        target.path(),
                        &[
                            "rev-parse",
                            "-q",
                            "--verify",
                            &format!("{}:{path}", found.id),
                        ],
                    );
                    let kept = output
                        .status
                        .success()
                        .then(|| String::from_utf8(output.stdout).unwrap().trim().to_owned());
                    assert_eq!(
                        kept.as_deref(),
                        blob,
                        "seed {seed}: {path} in {} is what {} saved",
                        found.id,
                        target.machine
                    );
                }
                true
            }
            code => panic!("seed {seed}: merge-tree failed with {code:?}: {text}"),
        }
    }
}

fn reflog(clone: &Clone) -> Vec<String> {
    let path = clone.path().join(".git/logs/refs/heads/main");
    fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}
