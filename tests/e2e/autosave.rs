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
    let lock = wiki.path().join(".git/index.lock");
    fs::write(&lock, "").unwrap();
    let reason = "index.lock exists: another Git command is running, or one was killed";
    assert_blocked(&mut wiki, reason, ".git");
    fs::remove_file(&lock).unwrap();
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
