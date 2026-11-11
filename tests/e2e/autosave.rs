//! The autosave service's refusals, one at a time, with a fake clock.

use std::fs::{self, File};
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mneme::autosave::{Autosave, Config, Outcome, Token};
use serde_json::Value;
use tempfile::TempDir;

use crate::git::Isolation;
use crate::server::{MACHINES, Server};

const QUIET: Duration = Duration::from_secs(15);
/// The fake clock starts at 2027-01-15T08:00:00Z.
const START: u64 = 1_800_000_000;

/// A server with twelve pages and one clone with its service.
struct Wiki {
    server: Server,
    git: Isolation,
    dir: TempDir,
    service: Autosave,
    now: SystemTime,
}

impl Wiki {
    fn new() -> Self {
        let git = Isolation::new();
        let server = Server::empty(&git);
        let (machine, token) = MACHINES[1];
        let header = format!("http.extraHeader=Authorization: Bearer {token}");
        let start = TempDir::new().unwrap();
        let url = server.git_url();
        git.ok(start.path(), &["-c", &header, "clone", "-q", &url, "."]);
        for page in 0..12 {
            fs::write(
                start.path().join(format!("page-{page}.md")),
                format!("# Page {page}\n\nline one\nline two\n"),
            )
            .unwrap();
        }
        git.ok(start.path(), &["add", "-A"]);
        git.ok(start.path(), &["commit", "-q", "-m", "Start"]);
        git.ok(
            start.path(),
            &["-c", &header, "push", "-q", "origin", "main"],
        );
        let dir = TempDir::new().unwrap();
        git.ok(dir.path(), &["-c", &header, "clone", "-q", &url, "."]);
        let service = Autosave::new(Config {
            folder: dir.path().to_owned(),
            server: server.url.clone(),
            token: Token(token.to_owned()),
            machine: machine.to_owned(),
            quiet: QUIET,
            fetch_every: Duration::from_secs(30),
            report_every: Duration::from_secs(300),
            stale_lock: Duration::from_secs(300),
        });
        Self {
            server,
            git,
            dir,
            service,
            now: UNIX_EPOCH + Duration::from_secs(START),
        }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    /// A tick after every file is quiet.
    fn tick(&mut self) -> Outcome {
        self.now += QUIET + Duration::from_secs(1);
        self.service.tick(self.now)
    }

    fn write(&self, path: &str, text: &str) {
        let file = self.path().join(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, text).unwrap();
        File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(self.now)
            .unwrap();
    }

    fn git(&self, args: &[&str]) -> Output {
        self.git.git(self.path(), args)
    }

    fn ok(&self, args: &[&str]) -> String {
        self.git.ok(self.path(), args)
    }

    fn head(&self) -> String {
        self.ok(&["rev-parse", "HEAD"]).trim().to_owned()
    }

    fn server_head(&self) -> String {
        self.git
            .ok(self.server.repo(), &["rev-parse", "main"])
            .trim()
            .to_owned()
    }

    fn pushes(&self) -> usize {
        self.server
            .log()
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|line| line["event"] == "push")
            .count()
    }
}

#[test]
fn a_save_that_would_delete_most_pages_stops_and_commits_nothing() {
    let mut wiki = Wiki::new();
    for page in 0..7 {
        fs::remove_file(wiki.path().join(format!("page-{page}.md"))).unwrap();
    }
    let head = wiki.head();
    assert_eq!(
        wiki.tick(),
        Outcome::Stopped("the save would delete 7 of 12 pages".to_owned())
    );
    assert_eq!(wiki.head(), head, "nothing was committed");
    assert_eq!(
        wiki.ok(&["diff", "--cached", "--name-only"]),
        "",
        "nothing was staged"
    );
    assert!(!wiki.path().join("page-0.md").exists(), "no file came back");
    assert_eq!(
        wiki.tick(),
        Outcome::Stopped("the save would delete 7 of 12 pages".to_owned())
    );

    wiki.ok(&["checkout", "--", "page-0.md"]);
    assert_eq!(
        wiki.tick(),
        Outcome::Pushed,
        "six of twelve is not more than half"
    );
    assert_eq!(wiki.server_head(), wiki.head());
}

#[test]
fn moving_every_page_into_a_folder_is_not_a_mass_delete() {
    let mut wiki = Wiki::new();
    fs::create_dir(wiki.path().join("archive")).unwrap();
    for page in 0..12 {
        let name = format!("page-{page}.md");
        fs::rename(
            wiki.path().join(&name),
            wiki.path().join("archive").join(&name),
        )
        .unwrap();
    }
    assert_eq!(wiki.tick(), Outcome::Pushed);
    let listing = wiki.ok(&["ls-tree", "-r", "--name-only", "HEAD"]);
    assert_eq!(
        listing
            .lines()
            .filter(|path| path.starts_with("archive/"))
            .count(),
        12
    );
}

#[test]
fn deleting_five_pages_saves_even_when_that_is_most_of_them() {
    let mut wiki = Wiki::new();
    for page in 0..7 {
        wiki.ok(&["rm", "-q", &format!("page-{page}.md")]);
    }
    wiki.ok(&["commit", "-q", "-m", "Keep five pages"]);
    assert_eq!(wiki.tick(), Outcome::Pushed);
    for page in 7..12 {
        fs::remove_file(wiki.path().join(format!("page-{page}.md"))).unwrap();
    }
    assert_eq!(wiki.tick(), Outcome::Pushed, "five is not more than five");
}

/// A state a person or another tool can leave the folder in: how to make it with stock Git,
/// and how to leave it again.
struct Case {
    reason: &'static str,
    enter: &'static [&'static [&'static str]],
    leave: &'static [&'static [&'static str]],
}

const CASES: [Case; 7] = [
    Case {
        reason: "HEAD is on side, not main",
        enter: &[&["switch", "-q", "side"]],
        leave: &[&["switch", "-q", "main"]],
    },
    Case {
        reason: "HEAD is detached, not on main",
        enter: &[&["switch", "-q", "--detach"]],
        leave: &[&["switch", "-q", "main"]],
    },
    Case {
        reason: "a merge is in progress",
        enter: &[&["merge", "side"]],
        leave: &[&["merge", "--abort"]],
    },
    Case {
        reason: "a cherry-pick is in progress",
        enter: &[&["cherry-pick", "side"]],
        leave: &[&["cherry-pick", "--abort"]],
    },
    Case {
        reason: "a revert is in progress",
        enter: &[&["revert", "--no-edit", "side"]],
        leave: &[&["revert", "--abort"]],
    },
    Case {
        reason: "a rebase is in progress",
        enter: &[&["stash", "-q", "-u"], &["rebase", "side"]],
        leave: &[&["rebase", "--abort"], &["stash", "pop", "-q"]],
    },
    Case {
        reason: "a bisect is in progress",
        enter: &[&["bisect", "start"]],
        leave: &[&["bisect", "reset"]],
    },
];

/// The pages the cases touch and the file that says where `HEAD` is, in `.git` or wherever it
/// was moved to.
fn snapshot(wiki: &Wiki, git_dir: &str) -> Vec<Option<Vec<u8>>> {
    ["page-1.md", "page-2.md", &format!("{git_dir}/HEAD")]
        .iter()
        .map(|path| fs::read(wiki.path().join(path)).ok())
        .collect()
}

/// Ticks in a blocked folder and checks that nothing changed, here or on the server.
fn assert_blocked(wiki: &mut Wiki, reason: &str, git_dir: &str) {
    let before = snapshot(wiki, git_dir);
    let server = wiki.server_head();
    assert_eq!(wiki.tick(), Outcome::Blocked(reason.to_owned()), "{reason}");
    assert_eq!(
        snapshot(wiki, git_dir),
        before,
        "{reason}: the folder is as it was"
    );
    assert_eq!(wiki.server_head(), server, "{reason}: nothing was pushed");
}

/// A local commit on main and an unsaved edit wait in the folder, so a service that ignored the
/// state would push.
#[test]
fn every_blocked_reason_leaves_the_folder_as_it_is() {
    let mut wiki = Wiki::new();
    wiki.write("page-1.md", "# Page 1\n\nline one\nfrom main\n");
    wiki.ok(&["commit", "-q", "-am", "Edit on main"]);
    wiki.ok(&["switch", "-q", "-c", "side", "HEAD~"]);
    wiki.write("page-1.md", "# Page 1\n\nline one\nfrom side\n");
    wiki.ok(&["commit", "-q", "-am", "Edit on side"]);
    wiki.ok(&["switch", "-q", "main"]);
    wiki.write("page-2.md", "# Page 2\n\nan unsaved edit\n");
    for case in &CASES {
        for args in case.enter {
            wiki.git(args);
        }
        assert_blocked(&mut wiki, case.reason, ".git");
        for args in case.leave {
            let output = wiki.git(args);
            assert!(
                output.status.success(),
                "{}: {args:?}: {output:?}",
                case.reason
            );
        }
    }
    wiki.write(".git/index.lock", "");
    let reason = "index.lock is 16 s old: another Git command is running, or one was killed";
    assert_blocked(&mut wiki, reason, ".git");
    fs::remove_file(wiki.path().join(".git/index.lock")).unwrap();
    fs::rename(wiki.path().join(".git"), wiki.path().join("git-away")).unwrap();
    assert_blocked(&mut wiki, "the folder is not a Git repository", "git-away");
    fs::rename(wiki.path().join("git-away"), wiki.path().join(".git")).unwrap();

    assert_eq!(
        wiki.tick(),
        Outcome::Pushed,
        "the service carries on once the folder is free"
    );
    assert_eq!(wiki.server_head(), wiki.head());
}

#[test]
fn a_push_the_server_refuses_blocks_until_the_head_changes() {
    let mut wiki = Wiki::new();
    std::os::unix::fs::symlink("page-1.md", wiki.path().join("link.md")).unwrap();
    wiki.ok(&["add", "link.md"]);
    wiki.ok(&["commit", "-q", "-m", "Link a page"]);
    let Outcome::Blocked(reason) = wiki.tick() else {
        panic!("the push is refused");
    };
    assert!(reason.contains("has a symlink at link.md"), "{reason}");
    let pushes = wiki.pushes();
    assert_eq!(wiki.tick(), Outcome::Blocked(reason.clone()));
    assert_eq!(wiki.tick(), Outcome::Blocked(reason.clone()));
    assert_eq!(wiki.pushes(), pushes, "no push until the head changes");

    wiki.write("page-3.md", "# Page 3\n\na new line\n");
    let Outcome::Blocked(again) = wiki.tick() else {
        panic!("the new head is refused too");
    };
    assert!(again.contains("has a symlink at link.md"), "{again}");
    assert_eq!(wiki.pushes(), pushes + 1, "a new head is pushed once");
}

/// `mneme autosave init` with the token of `blackbox`.
fn init(git: &Isolation, server: &Server, folder: &Path) -> Output {
    let tokens = TempDir::new().unwrap();
    let token_file = tokens.path().join("token");
    fs::write(&token_file, format!("{}\n", MACHINES[1].1)).unwrap();
    git.apply(
        Command::new(env!("CARGO_BIN_EXE_mneme"))
            .args(["autosave", "init", "--folder"])
            .arg(folder)
            .args(["--server", &server.url, "--token-file"])
            .arg(&token_file)
            .args(["--name", MACHINES[1].0]),
    )
    .output()
    .unwrap()
}

fn assert_cloned(git: &Isolation, server: &Server, folder: &Path) {
    assert!(folder.join("page-0.md").exists());
    assert_eq!(
        git.ok(folder, &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/main"
    );
    assert_eq!(
        git.ok(folder, &["remote", "get-url", "origin"]).trim(),
        server.git_url()
    );
    let config = fs::read_to_string(folder.join(".git/config")).unwrap();
    assert!(
        !config.contains(MACHINES[1].1),
        "the token is not in the repository's config"
    );
}

#[test]
fn init_clones_into_a_missing_or_empty_folder() {
    let wiki = Wiki::new();
    let root = TempDir::new().unwrap();
    let missing = root.path().join("deep/wiki");
    let output = init(&wiki.git, &wiki.server, &missing);
    assert!(output.status.success(), "{output:?}");
    assert_cloned(&wiki.git, &wiki.server, &missing);

    let empty = root.path().join("empty");
    fs::create_dir(&empty).unwrap();
    let output = init(&wiki.git, &wiki.server, &empty);
    assert!(output.status.success(), "{output:?}");
    assert_cloned(&wiki.git, &wiki.server, &empty);
}

#[test]
fn init_adds_or_checks_the_origin_of_an_existing_repository() {
    let wiki = Wiki::new();
    let existing = TempDir::new().unwrap();
    wiki.git.ok(existing.path(), &["init", "-q", "-b", "main"]);
    fs::write(existing.path().join("mine.md"), "# Mine\n").unwrap();
    let output = init(&wiki.git, &wiki.server, existing.path());
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        wiki.git
            .ok(existing.path(), &["remote", "get-url", "origin"])
            .trim(),
        wiki.server.git_url()
    );
    assert_eq!(
        fs::read_to_string(existing.path().join("mine.md")).unwrap(),
        "# Mine\n"
    );
    let again = init(&wiki.git, &wiki.server, existing.path());
    assert!(again.status.success(), "init can run again: {again:?}");

    wiki.git.ok(
        existing.path(),
        &["remote", "set-url", "origin", "http://elsewhere/wiki.git"],
    );
    let output = init(&wiki.git, &wiki.server, existing.path());
    assert!(!output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("not http://"),
        "{output:?}"
    );
}

#[test]
fn init_refuses_a_folder_with_files_that_is_not_a_repository() {
    let wiki = Wiki::new();
    let folder = TempDir::new().unwrap();
    fs::write(folder.path().join("notes.md"), "# Notes\n").unwrap();
    let output = init(&wiki.git, &wiki.server, folder.path());
    assert!(!output.status.success());
    assert!(!folder.path().join(".git").exists());
    assert_eq!(
        fs::read_to_string(folder.path().join("notes.md")).unwrap(),
        "# Notes\n"
    );
}

/// The server's main moves while the push is being checked, as when two machines push at once:
/// the server's own hook moves it to a commit made elsewhere, the first time it runs. The push
/// loses the race, which is normal, so the next tick replays onto the new main and pushes.
#[test]
fn a_push_that_loses_a_race_on_the_server_is_tried_again() {
    let mut wiki = Wiki::new();
    let repo = wiki.server.repo().to_owned();
    let other = TempDir::new().unwrap();
    let header = format!("http.extraHeader=Authorization: Bearer {}", MACHINES[0].1);
    let url = wiki.server.git_url();
    wiki.git
        .ok(other.path(), &["-c", &header, "clone", "-q", &url, "."]);
    fs::write(
        other.path().join("page-9.md"),
        "# Page 9\n\nwritten elsewhere\n",
    )
    .unwrap();
    wiki.git
        .ok(other.path(), &["commit", "-q", "-am", "Edit elsewhere"]);
    let other_path = other.path().to_str().unwrap().to_owned();
    wiki.git
        .ok(&repo, &["fetch", "-q", &other_path, "main:refs/raced"]);
    let hook = format!(
        "#!/bin/sh\nif [ ! -e raced ]; then\n  touch raced\n  \
         env -u GIT_QUARANTINE_PATH -u GIT_OBJECT_DIRECTORY -u GIT_ALTERNATE_OBJECT_DIRECTORIES \
         git update-ref refs/heads/main refs/raced\nfi\nexec '{}' hook pre-receive\n",
        env!("CARGO_BIN_EXE_mneme")
    );
    fs::write(repo.join("hooks/pre-receive"), hook).unwrap();

    wiki.write("page-3.md", "# Page 3\n\nwritten here\n");
    let first = wiki.tick();
    assert!(
        !matches!(first, Outcome::Blocked(_)),
        "losing a race is not a refusal: {first:?}"
    );
    assert!(repo.join("raced").exists(), "the race happened");
    assert_eq!(
        wiki.tick(),
        Outcome::Pushed,
        "the next tick replays and pushes"
    );
    assert_eq!(wiki.server_head(), wiki.head());
    let page = wiki.ok(&["show", "HEAD:page-9.md"]);
    assert_eq!(
        page, "# Page 9\n\nwritten elsewhere\n",
        "the other machine's edit is kept"
    );
}

/// `git update-index --index-info` takes `index.lock` and then waits for its input, so killing
/// it with SIGKILL leaves the lock behind as a crash would.
fn leave_a_lock_by_killing_git(wiki: &Wiki) {
    let mut child = wiki
        .git
        .apply(
            Command::new("git")
                .current_dir(wiki.path())
                .args(["update-index", "--index-info"]),
        )
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let lock = wiki.path().join(".git/index.lock");
    while !lock.exists() {
        std::thread::yield_now();
    }
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(lock.exists(), "SIGKILL left the lock");
    File::options()
        .write(true)
        .open(&lock)
        .unwrap()
        .set_modified(wiki.now)
        .unwrap();
}

#[test]
fn a_lock_left_by_a_killed_git_command_goes_once_it_is_stale() {
    let mut wiki = Wiki::new();
    wiki.write("page-1.md", "# Page 1\n\nan unsaved edit\n");
    leave_a_lock_by_killing_git(&wiki);
    let head = wiki.head();
    let Outcome::Blocked(reason) = wiki.tick() else {
        panic!("a lock younger than the stale time blocks the tick");
    };
    assert!(reason.starts_with("index.lock is 16 s old"), "{reason}");
    assert!(
        wiki.path().join(".git/index.lock").exists(),
        "a young lock is left alone"
    );
    assert_eq!(wiki.head(), head);

    wiki.now += Duration::from_secs(300);
    assert_eq!(
        wiki.tick(),
        Outcome::Pushed,
        "the stale lock goes and the save goes through"
    );
    assert!(!wiki.path().join(".git/index.lock").exists());
    assert_eq!(
        wiki.ok(&["show", "HEAD:page-1.md"]),
        "# Page 1\n\nan unsaved edit\n"
    );
    assert_eq!(wiki.server_head(), wiki.head());
}

#[test]
fn a_lock_that_keeps_being_written_is_never_removed() {
    let mut wiki = Wiki::new();
    wiki.write("page-1.md", "# Page 1\n\nan unsaved edit\n");
    let head = wiki.head();
    for _ in 0..40 {
        wiki.write(".git/index.lock", "a command still writing");
        let outcome = wiki.tick();
        assert!(matches!(outcome, Outcome::Blocked(_)), "{outcome:?}");
    }
    assert!(wiki.now > UNIX_EPOCH + Duration::from_secs(START + 600));
    assert!(wiki.path().join(".git/index.lock").exists());
    assert_eq!(wiki.head(), head, "nothing was committed under a live lock");
}

#[test]
fn an_index_a_dead_command_left_unreadable_is_read_again_from_head() {
    let mut wiki = Wiki::new();
    wiki.write("page-1.md", "# Page 1\n\nan unsaved edit\n");
    wiki.write("new.md", "# New\n\nnot saved yet\n");
    leave_a_lock_by_killing_git(&wiki);
    fs::write(wiki.path().join(".git/index"), "half an index").unwrap();
    assert!(
        !wiki.git(&["status"]).status.success(),
        "the index cannot be read"
    );

    wiki.now += Duration::from_secs(300);
    assert_eq!(wiki.tick(), Outcome::Pushed);
    assert_eq!(
        fs::read_to_string(wiki.path().join("page-1.md")).unwrap(),
        "# Page 1\n\nan unsaved edit\n",
        "the work tree was not touched"
    );
    assert_eq!(
        wiki.ok(&["show", "HEAD:page-1.md"]),
        "# Page 1\n\nan unsaved edit\n"
    );
    assert_eq!(
        wiki.ok(&["show", "HEAD:new.md"]),
        "# New\n\nnot saved yet\n"
    );
    assert_eq!(wiki.ok(&["status", "--porcelain"]), "");
}

/// Seed 24 of the simulation. A hand commit leaves nothing to save, the server has moved, and a
/// dead command left a lock and half an index. The rebuilt index holds no file stats, and
/// `git reset --keep` refuses to move a file whose entry is out of date, so the service must
/// store them first.
#[test]
fn a_replay_right_after_the_index_is_rebuilt_moves_the_folder() {
    let mut wiki = Wiki::new();
    let other = TempDir::new().unwrap();
    let header = format!("http.extraHeader=Authorization: Bearer {}", MACHINES[0].1);
    let url = wiki.server.git_url();
    wiki.git
        .ok(other.path(), &["-c", &header, "clone", "-q", &url, "."]);
    fs::write(
        other.path().join("page-5.md"),
        "# Page 5\n\nfrom elsewhere\n",
    )
    .unwrap();
    wiki.git
        .ok(other.path(), &["commit", "-q", "-am", "Edit elsewhere"]);
    wiki.git.ok(
        other.path(),
        &["-c", &header, "push", "-q", "origin", "main"],
    );
    wiki.write("page-1.md", "# Page 1\n\nby hand\n");
    wiki.ok(&["commit", "-q", "-am", "Edit by hand"]);
    leave_a_lock_by_killing_git(&wiki);
    fs::write(wiki.path().join(".git/index"), "half an index").unwrap();

    wiki.now += Duration::from_secs(300);
    assert_eq!(wiki.tick(), Outcome::Pushed);
    assert_eq!(wiki.server_head(), wiki.head());
    assert_eq!(
        fs::read_to_string(wiki.path().join("page-5.md")).unwrap(),
        "# Page 5\n\nfrom elsewhere\n"
    );
    assert_eq!(wiki.ok(&["status", "--porcelain"]), "");
}
