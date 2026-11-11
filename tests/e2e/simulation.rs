//! The gate for the autosave service: three clones, each with its own service, against one real
//! server. The test drives every tick with a fake clock, in an order a seeded generator picks,
//! and between ticks does what writers do. `MNEME_SIM_SEEDS` sets how many seeds run,
//! `MNEME_SIM_SEED` runs one seed, and `MNEME_SIM_STEPS` sets the steps per seed.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::fs::{self, File};
use std::panic::{self, AssertUnwindSafe};
use std::path::Path;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mneme::autosave::{Autosave, Config, Outcome, Token};
use tempfile::TempDir;

use crate::git::Isolation;
use crate::server::{MACHINES, Server};

const QUIET: Duration = Duration::from_secs(15);
const FETCH_EVERY: Duration = Duration::from_secs(30);
/// The fake clock starts at 2027-01-15T08:00:00Z.
const START: u64 = 1_800_000_000;
/// Port 9 is discard, which nothing on the test machine listens on.
const UNREACHABLE: &str = "http://127.0.0.1:9/git/wiki.git";
const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const GATE_SEEDS: u64 = 64;
const GATE_STEPS: usize = 80;

fn env_number<T: std::str::FromStr>(name: &str) -> Option<T> {
    std::env::var(name).ok().map(|value| {
        value
            .parse()
            .unwrap_or_else(|_| panic!("{name} is not a number"))
    })
}

#[test]
#[ignore = "the autosave tick is a stub until the loop lands"]
fn three_clones_end_equal_to_the_server_with_every_commit_once() {
    let steps = env_number("MNEME_SIM_STEPS").unwrap_or(GATE_STEPS);
    let seeds: Vec<u64> = match env_number("MNEME_SIM_SEED") {
        Some(seed) => vec![seed],
        None => (0..env_number("MNEME_SIM_SEEDS").unwrap_or(GATE_SEEDS)).collect(),
    };
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
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
                    let mut sim = Sim::new(seed);
                    let run = panic::catch_unwind(AssertUnwindSafe(|| sim.run(steps)));
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
    eprintln!("{} seeds of {steps} steps passed", seeds.len());
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

    fn status(&self) -> String {
        self.ok(&["status", "--porcelain", "--untracked-files=all"])
    }

    fn pages(&self) -> Vec<String> {
        let mut pages = Vec::new();
        walk(self.path(), "", &mut pages);
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
    message: String,
}

impl Commit {
    fn read(git: &Isolation, repo: &Path, id: &str) -> Self {
        let format = "--format=%H%x00%an%x00%ae%x00%at%x00%P%x00%B";
        let text = git.ok(repo, &["log", "-1", format, id]);
        let fields: Vec<&str> = text.splitn(6, '\0').collect();
        Self {
            id: fields[0].to_owned(),
            author: fields[1].to_owned(),
            email: fields[2].to_owned(),
            time: fields[3].to_owned(),
            parents: fields[4].split_whitespace().map(str::to_owned).collect(),
            message: fields[5].trim_end().to_owned(),
        }
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
    server: Server,
    git: Isolation,
    clones: Vec<Clone>,
    now: SystemTime,
    counter: u64,
    /// Every commit a clone made, by clone, from its reflog.
    made: Vec<(usize, String)>,
    outcomes: Vec<(usize, Outcome)>,
    trace: Vec<String>,
    /// The server's head once the starting pages are in.
    base: String,
}

impl Sim {
    fn new(seed: u64) -> Self {
        let git = Isolation::new();
        let server = Server::empty(&git);
        let now = UNIX_EPOCH + Duration::from_secs(START);
        let mut sim = Self {
            seed,
            rng: fastrand::Rng::with_seed(seed),
            server,
            git,
            clones: Vec::new(),
            now,
            counter: 0,
            made: Vec::new(),
            outcomes: Vec::new(),
            trace: Vec::new(),
            base: String::new(),
        };
        sim.start_pages();
        for (machine, token) in MACHINES {
            let clone = sim.clone_as(machine, token);
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

    fn clone_as(&self, machine: &'static str, token: &'static str) -> Clone {
        let dir = TempDir::new().unwrap();
        let git = Isolation::new();
        let header = format!("http.extraHeader=Authorization: Bearer {token}");
        let url = self.server.git_url();
        git.ok(dir.path(), &["-c", &header, "clone", "-q", &url, "."]);
        git.ok(dir.path(), &["config", "gc.auto", "0"]);
        let service = Autosave::new(Config {
            folder: dir.path().to_owned(),
            server: self.server.url.clone(),
            token: Token(token.to_owned()),
            machine: machine.to_owned(),
            quiet: QUIET,
            fetch_every: FETCH_EVERY,
            report_every: Duration::from_secs(300),
        });
        let mut clone = Clone {
            machine,
            token,
            dir,
            git,
            service,
            online: true,
            reflog: 0,
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
            0..40 => self.tick(clone),
            40..50 => self.create(clone),
            50..62 => self.edit(clone),
            62..67 => self.append(clone),
            67..71 => self.delete(clone),
            71..75 => self.rename(clone, false),
            75..79 => self.rename(clone, true),
            79..85 => self.same_line_edits(clone),
            85..90 => self.hand_commit(clone),
            90..94 => self.hand_pull(clone),
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
        self.outcomes.push((clone, outcome.clone()));
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

    /// Two clones that both have a page as the server has it change the same line of it, and
    /// both save before either syncs. Whichever integrates second overlaps, keeps its own
    /// text and says so.
    fn same_line_edits(&mut self, first: usize) {
        let second = (first + 1 + self.rng.usize(..self.clones.len() - 1)) % self.clones.len();
        if !self.clones[first].online || !self.clones[second].online {
            return;
        }
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
                    && self.server_text(&server_head, path).as_deref() == Some(&text[..])
            })
            .collect();
        if candidates.is_empty() {
            return;
        }
        let path = candidates[self.rng.usize(..candidates.len())].clone();
        let lines = self.read(first, &path).lines().count();
        let line = self.rng.usize(..lines.max(1));
        self.replace_line(first, &path, Some(line));
        let saved = self.replace_line(second, &path, Some(line));
        self.advance(QUIET + Duration::from_secs(1));
        self.tick_now(first);
        let outcome = self.tick_now(second);
        let seed = self.seed;
        assert_eq!(
            outcome,
            Outcome::Pushed,
            "seed {seed}: the second clone pushes its overlapping save"
        );
        let head = self.server_head();
        let commit = Commit::read(&self.git, self.server.repo(), &head);
        assert_eq!(
            commit.overlaps(),
            1,
            "seed {seed}: {head} has one Overlap trailer: {}",
            commit.message
        );
        assert_eq!(
            self.server_text(&head, &path).as_deref(),
            Some(saved.as_bytes()),
            "seed {seed}: {path} has the text the second clone saved"
        );
    }

    fn hand_commit(&mut self, clone: usize) {
        if self.clones[clone].status().is_empty() {
            return;
        }
        let token = self.token(clone);
        let message = format!("Hand edit {token}\n\nAn agent committed this by hand.");
        let now = self.now;
        let target = &self.clones[clone];
        target.ok(&["add", "-A"]);
        let output = target.git_at(now, &["commit", "-q", "-m", &message]);
        assert!(output.status.success(), "{output:?}");
        self.note(clone, &format!("hand commit {token}"));
        self.read_reflog(clone);
    }

    /// `git pull` with a merge, as an agent might run it. A pull that stops on a conflict is
    /// undone, as the agent would before walking away.
    fn hand_pull(&mut self, clone: usize) {
        if !self.clones[clone].online {
            return;
        }
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
        self.read_reflog(clone);
    }

    fn toggle_network(&mut self, clone: usize) {
        let online = !self.clones[clone].online;
        let url = if online {
            self.server.git_url()
        } else {
            UNREACHABLE.to_owned()
        };
        self.clones[clone].ok(&["config", "remote.origin.url", &url]);
        self.clones[clone].online = online;
        self.note(clone, if online { "online" } else { "offline" });
    }

    fn server_head(&self) -> String {
        self.git
            .ok(self.server.repo(), &["rev-parse", "main"])
            .trim()
            .to_owned()
    }

    fn server_text(&self, commit: &str, path: &str) -> Option<Vec<u8>> {
        let output = self.git.git(
            self.server.repo(),
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
        let merges = self
            .git
            .ok(self.server.repo(), &["rev-list", "--min-parents=2", "main"]);
        assert_eq!(merges, "", "seed {seed}: the server's history is one line");
        let stuck: Vec<_> = self
            .outcomes
            .iter()
            .filter(|(_, outcome)| matches!(outcome, Outcome::Blocked(_) | Outcome::Stopped(_)))
            .collect();
        assert!(
            stuck.is_empty(),
            "seed {seed}: no tick was blocked or stopped: {stuck:?}"
        );
        self.check_every_commit_once();
    }

    fn check_every_commit_once(&self) {
        let seed = self.seed;
        let repo = self.server.repo();
        let range = format!("{}..main", self.base);
        let on_server: Vec<Commit> = self
            .git
            .ok(repo, &["rev-list", "--reverse", &range])
            .lines()
            .map(|id| Commit::read(&self.git, repo, id))
            .collect();
        let mut by_key: HashMap<_, Vec<&Commit>> = HashMap::new();
        for commit in &on_server {
            by_key.entry(commit.key()).or_default().push(commit);
        }
        let made: Vec<(usize, Commit)> = self
            .made
            .iter()
            .map(|(clone, id)| {
                (
                    *clone,
                    Commit::read(&self.clones[*clone].git, self.clones[*clone].path(), id),
                )
            })
            .filter(|(_, commit)| commit.parents.len() <= 1)
            .collect();
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
        for (clone, commit) in &made {
            match by_key.get(&commit.key()) {
                Some(found) => {
                    let found = found[0];
                    assert_eq!(found.email, commit.email, "seed {seed}: the author is kept");
                    assert!(
                        found.message.starts_with(&commit.message),
                        "seed {seed}: the message of {} is kept in {}",
                        commit.id,
                        found.id
                    );
                    self.check_overlap(*clone, commit, found);
                }
                None => self.check_already_there(*clone, commit),
            }
        }
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
                self.server.repo(),
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
    /// When it does not, main's commit is exactly that merge.
    fn check_overlap(&self, clone: usize, made: &Commit, found: &Commit) {
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
        let tree = target.ok(&["rev-parse", &format!("{}^{{tree}}", found.id)]);
        match output.status.code() {
            Some(0) => {
                assert_eq!(
                    found.overlaps(),
                    made.overlaps(),
                    "seed {seed}: {} did not overlap and has no Overlap trailer",
                    found.id
                );
                assert_eq!(
                    tree.trim(),
                    merged,
                    "seed {seed}: {} is Git's merge",
                    found.id
                );
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
