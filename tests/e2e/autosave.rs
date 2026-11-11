//! The autosave service's refusals, one at a time, with a fake clock.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mneme::autosave::{Autosave, Config, Outcome, Token};
use mneme::rules::{MAX_FILE, MAX_PAGE};
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::git::{self, Isolation};
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
    _racy: Option<TempDir>,
}

impl Wiki {
    fn new() -> Self {
        Self::with_git("git".into(), None)
    }

    /// A clone whose service runs [`git::racy_git`].
    fn racing() -> Self {
        let dir = TempDir::new().unwrap();
        let program = git::racy_git(dir.path());
        Self::with_git(program, Some(dir))
    }

    fn with_git(program: PathBuf, racy: Option<TempDir>) -> Self {
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
            git: program,
        });
        Self {
            server,
            git,
            dir,
            service,
            now: UNIX_EPOCH + Duration::from_secs(START),
            _racy: racy,
        }
    }

    /// Makes an agent's hand commit of a new page land right before the service's next Git
    /// command that includes `before`.
    fn race_before(&self, before: &str) {
        let script = format!(
            "export GIT_AUTHOR_NAME=Agent GIT_AUTHOR_EMAIL=agent@example.invalid \\
             GIT_AUTHOR_DATE='@{START} +0000'\n\
             printf '# Hand\\n\\nwritten during the move\\n' > \"$GIT_WORK_TREE/hand.md\"\n\
             git add -- hand.md && git commit -q -m 'Hand edit during the move'\n"
        );
        self.run_before(before, &script);
    }

    /// Runs the shell `script` right before the service's next Git command that includes
    /// `before`.
    fn run_before(&self, before: &str, script: &str) {
        fs::write(self.path().join(".git/mneme-test-race"), script).unwrap();
        fs::write(self.path().join(".git/mneme-test-race.before"), before).unwrap();
    }

    fn raced(&self) -> bool {
        self.path().join(".git/mneme-test-race.ran").exists()
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

    /// What `/v1/machines` says of this clone's machine.
    fn reported(&self) -> Value {
        let listing = self.server.json("/v1/machines");
        listing["machines"]
            .as_array()
            .unwrap()
            .iter()
            .find(|machine| machine["name"] == MACHINES[1].0)
            .unwrap()
            .clone()
    }

    /// Commits `text` to `path` on another clone and pushes it.
    fn push_from_elsewhere(&self, path: &str, text: &str) {
        let other = TempDir::new().unwrap();
        let header = format!("http.extraHeader=Authorization: Bearer {}", MACHINES[0].1);
        let url = self.server.git_url();
        self.git
            .ok(other.path(), &["-c", &header, "clone", "-q", &url, "."]);
        fs::write(other.path().join(path), text).unwrap();
        self.git.ok(other.path(), &["add", "-A"]);
        self.git
            .ok(other.path(), &["commit", "-q", "-m", "Edit elsewhere"]);
        self.git.ok(
            other.path(),
            &["-c", &header, "push", "-q", "origin", "main"],
        );
    }

    /// Whether `path` is in the commit `at`.
    fn has(&self, at: &str, path: &str) -> bool {
        self.git(&["cat-file", "-e", &format!("{at}:{path}")])
            .status
            .success()
    }

    /// The path of every request the server logged with this clone's token.
    fn requests(&self) -> Vec<String> {
        self.server
            .log_with("/v1/machines")
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|line| line["event"] == "request" && line["machine"] == MACHINES[1].0)
            .map(|line| line["path"].as_str().unwrap().to_owned())
            .collect()
    }
}

/// An agent commits by hand right before the service moves the folder to the server's main,
/// both when the service replays its commits and when it only takes the server's. The hand
/// commit stays, with its author and message, and reaches the server.
#[test]
fn a_hand_commit_that_lands_as_the_folder_moves_is_kept() {
    let points: [(&str, bool); 4] = [
        ("read-tree -m", true),
        ("update-ref", true),
        ("read-tree -m", false),
        ("update-ref", false),
    ];
    for (before, diverged) in points {
        let mut wiki = Wiki::racing();
        wiki.push_from_elsewhere("page-5.md", "# Page 5\n\nfrom elsewhere\n");
        if diverged {
            wiki.write("page-3.md", "# Page 3\n\nby hand before\n");
            wiki.ok(&["commit", "-q", "-am", "Edit by hand"]);
        }
        wiki.race_before(before);
        let outcome = wiki.tick();
        assert!(wiki.raced(), "{before}: the race happened");
        assert_eq!(outcome, Outcome::Pushed, "{before}");
        assert_eq!(wiki.server_head(), wiki.head(), "{before}");
        let subjects = wiki.ok(&["log", "--format=%an %s", "-4"]);
        assert!(
            subjects.contains("Agent Hand edit during the move"),
            "{before}: the hand commit is on main: {subjects}"
        );
        assert_eq!(
            fs::read_to_string(wiki.path().join("hand.md")).unwrap(),
            "# Hand\n\nwritten during the move\n",
            "{before}"
        );
        assert!(
            wiki.ok(&["show", "HEAD:page-5.md"])
                .contains("from elsewhere"),
            "{before}"
        );
        if diverged {
            assert!(
                wiki.ok(&["show", "HEAD:page-3.md"])
                    .contains("by hand before"),
                "{before}"
            );
        }
        assert_eq!(wiki.ok(&["status", "--porcelain"]), "", "{before}");
    }
}

/// A service killed after it moved the folder and before it moved `main` leaves the server's
/// changes as staged changes. The next tick saves them, its replay finds them already on the
/// server and leaves that commit out, and nothing is lost on either side.
#[test]
fn a_folder_that_moved_without_its_main_loses_nothing() {
    let mut wiki = Wiki::new();
    wiki.push_from_elsewhere("page-5.md", "# Page 5\n\nfrom elsewhere\n");
    wiki.write("page-3.md", "# Page 3\n\nby hand\n");
    wiki.ok(&["commit", "-q", "-am", "Edit by hand"]);
    let header = format!("http.extraHeader=Authorization: Bearer {}", MACHINES[1].1);
    wiki.ok(&["-c", &header, "fetch", "-q", "origin"]);
    let merged = wiki.ok(&[
        "merge-tree",
        "--write-tree",
        "--merge-base=HEAD^",
        "origin/main",
        "HEAD",
    ]);
    let replayed = wiki.ok(&[
        "commit-tree",
        "-p",
        "origin/main",
        "-m",
        "Edit by hand",
        merged.trim(),
    ]);
    wiki.ok(&["read-tree", "-m", "-u", "HEAD", replayed.trim()]);
    wiki.write("page-7.md", "# Page 7\n\nwritten after the kill\n");
    for _ in 0..3 {
        let outcome = wiki.tick();
        assert!(
            !matches!(outcome, Outcome::Blocked(_) | Outcome::Stopped(_)),
            "{outcome:?}"
        );
    }
    assert_eq!(wiki.server_head(), wiki.head());
    assert_eq!(wiki.ok(&["status", "--porcelain"]), "");
    for (page, text) in [
        (3, "by hand"),
        (5, "from elsewhere"),
        (7, "written after the kill"),
    ] {
        let shown = wiki.ok(&["show", &format!("HEAD:page-{page}.md")]);
        assert!(shown.contains(text), "page-{page}.md: {shown}");
    }
    assert_eq!(
        wiki.ok(&["log", "--format=%s", "--", "page-5.md"]),
        "Edit elsewhere\nStart\n",
        "no commit of this clone touches page 5 again"
    );
}

/// The report travels on Git's own HTTP path, so it trusts the same certificates as syncing
/// does, and it still goes out when the folder guard stops the tick before any fetch.
#[test]
fn the_service_reports_its_state_with_a_git_request() {
    let mut wiki = Wiki::new();
    wiki.write("page-1.md", "# Page 1\n\nan edit\n");
    assert_eq!(wiki.tick(), Outcome::Pushed);
    let reported = wiki.reported();
    assert_eq!(reported["state"], "pushed");
    assert_eq!(reported["head"], wiki.head().as_str());
    assert_eq!(reported["current"], true);

    fs::rename(wiki.path().join(".git"), wiki.path().join("git-away")).unwrap();
    let reason = "the folder is not a Git repository";
    assert_eq!(wiki.tick(), Outcome::Blocked(reason.to_owned()));
    let reported = wiki.reported();
    assert_eq!(reported["state"], "blocked");
    assert_eq!(reported["detail"], reason);
    assert_eq!(reported["head"], Value::Null);

    let requests = wiki.requests();
    assert!(
        requests.len() >= 3,
        "a fetch, a push and reports: {requests:?}"
    );
    for path in requests {
        assert!(
            path.starts_with("/git/wiki.git/"),
            "{path} is not a Git route"
        );
    }
}

/// A file of `size` bytes that takes no room on disk.
fn sparse(path: &Path, size: u64) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    File::create(path).unwrap().set_len(size).unwrap();
}

/// One file of each kind the server refuses or Git cannot store, with the reason the service
/// gives for leaving it out, by path.
fn write_bad_files(wiki: &Wiki) -> Vec<(String, &'static str)> {
    let folder = wiki.path();
    std::os::unix::fs::symlink("page-1.md", folder.join("link.md")).unwrap();
    sparse(&folder.join("big.md"), MAX_PAGE + 1);
    sparse(&folder.join("huge.bin"), MAX_FILE + 1);
    fs::create_dir(folder.join("nested")).unwrap();
    wiki.git.ok(&folder.join("nested"), &["init", "-q"]);
    fs::create_dir(folder.join(".GIT")).unwrap();
    fs::write(folder.join(".GIT/config"), "[core]\n").unwrap();
    fs::create_dir_all(folder.join("odd/git~1")).unwrap();
    fs::write(folder.join("odd/git~1/notes.md"), "# Notes\n").unwrap();
    let mut bad = vec![
        (".GIT/config".to_owned(), "a path named .git"),
        ("big.md".to_owned(), "a page over 1 MiB"),
        ("huge.bin".to_owned(), "a file over 10 MiB"),
        ("link.md".to_owned(), "a symlink"),
        ("nested/".to_owned(), "a submodule or nested repository"),
        ("odd/git~1/notes.md".to_owned(), "a name Git will not store"),
    ];
    bad.sort();
    bad
}

fn skipped(entries: &[(String, &str)]) -> Value {
    entries
        .iter()
        .map(|(path, reason)| json!({ "path": path, "reason": reason }))
        .collect()
}

/// The good edit beside them is saved and pushed, and the bad files stay in the folder, out of
/// history, listed in the report. They are not unsaved edits, so the service still replays.
#[test]
fn files_the_server_would_refuse_stay_in_the_folder_and_the_rest_is_saved() {
    let mut wiki = Wiki::new();
    let bad = write_bad_files(&wiki);
    wiki.write("page-2.md", "# Page 2\n\na good edit\n");
    assert_eq!(wiki.tick(), Outcome::Pushed);
    let head = wiki.head();
    assert_eq!(wiki.server_head(), head);
    assert_eq!(
        wiki.ok(&["show", "HEAD:page-2.md"]),
        "# Page 2\n\na good edit\n"
    );
    for (path, _) in &bad {
        assert!(
            !wiki.has("HEAD", path.trim_end_matches('/')),
            "{path} is not saved"
        );
        assert!(
            fs::symlink_metadata(wiki.path().join(path)).is_ok(),
            "{path} is still in the folder"
        );
    }
    let reported = wiki.reported();
    assert_eq!(reported["skipped"], skipped(&bad));
    assert_eq!(reported["skipped_more"], 0);
    assert_eq!(wiki.tick(), Outcome::Idle, "nothing is left to save");

    wiki.push_from_elsewhere("page-5.md", "# Page 5\n\nfrom elsewhere\n");
    wiki.write("page-3.md", "# Page 3\n\nby hand\n");
    wiki.ok(&["commit", "-q", "-am", "Edit by hand"]);
    assert_eq!(wiki.tick(), Outcome::Pushed, "the hand commit is replayed");
    assert_eq!(wiki.server_head(), wiki.head());
    assert!(
        wiki.ok(&["show", "HEAD:page-5.md"])
            .contains("from elsewhere")
    );
    assert!(wiki.ok(&["show", "HEAD:page-3.md"]).contains("by hand"));
    assert_eq!(wiki.reported()["skipped"], skipped(&bad));
}

/// A page that becomes a symlink or grows over the limit keeps its last good version in
/// history until it is fixed.
#[test]
fn a_tracked_page_that_turns_bad_keeps_its_last_good_version() {
    let mut wiki = Wiki::new();
    fs::remove_file(wiki.path().join("page-2.md")).unwrap();
    std::os::unix::fs::symlink("page-1.md", wiki.path().join("page-2.md")).unwrap();
    sparse(&wiki.path().join("page-3.md"), MAX_PAGE + 1);
    wiki.write("page-4.md", "# Page 4\n\na good edit\n");
    assert_eq!(wiki.tick(), Outcome::Pushed);
    for page in [2, 3] {
        assert_eq!(
            wiki.ok(&["show", &format!("HEAD:page-{page}.md")]),
            format!("# Page {page}\n\nline one\nline two\n")
        );
    }
    assert!(wiki.ok(&["show", "HEAD:page-4.md"]).contains("a good edit"));
    let bad = [
        ("page-2.md".to_owned(), "a symlink"),
        ("page-3.md".to_owned(), "a page over 1 MiB"),
    ];
    assert_eq!(wiki.reported()["skipped"], skipped(&bad));

    fs::remove_file(wiki.path().join("page-2.md")).unwrap();
    wiki.write("page-2.md", "# Page 2\n\nfixed\n");
    assert_eq!(wiki.tick(), Outcome::Pushed);
    assert!(wiki.ok(&["show", "HEAD:page-2.md"]).contains("fixed"));
    assert_eq!(wiki.reported()["skipped"], skipped(&bad[1..]));
}

/// When the server changes a page the folder leaves out, or adds a page where the folder has a
/// file it leaves out, the folder cannot move without overwriting it. That needs a person, so
/// the service says which files are in the way on every tick until they are fixed.
#[test]
fn a_clone_stuck_behind_a_left_out_file_is_blocked_until_it_is_fixed() {
    let mut wiki = Wiki::new();
    sparse(&wiki.path().join("page-3.md"), MAX_PAGE + 1);
    std::os::unix::fs::symlink("page-1.md", wiki.path().join("new.md")).unwrap();
    assert_eq!(wiki.tick(), Outcome::Idle);
    let head = wiki.head();
    wiki.push_from_elsewhere("page-3.md", "# Page 3\n\nfrom elsewhere\n");
    wiki.push_from_elsewhere("new.md", "# New\n\nfrom elsewhere\n");
    let both = "the server changed files the service leaves out, so the folder cannot take its \
                changes until they are fixed: new.md (a symlink), page-3.md (a page over 1 MiB)";
    assert_eq!(wiki.tick(), Outcome::Idle, "no fetch is due yet");
    assert_eq!(wiki.tick(), Outcome::Blocked(both.to_owned()));
    assert_eq!(
        wiki.tick(),
        Outcome::Blocked(both.to_owned()),
        "a tick without a fetch still says so"
    );
    assert_eq!(wiki.head(), head);
    assert_eq!(
        fs::metadata(wiki.path().join("page-3.md")).unwrap().len(),
        MAX_PAGE + 1
    );
    let reported = wiki.reported();
    assert_eq!(reported["state"], "blocked");
    assert_eq!(reported["detail"], both);
    assert_eq!(
        reported["skipped"],
        skipped(&[
            ("new.md".to_owned(), "a symlink"),
            ("page-3.md".to_owned(), "a page over 1 MiB"),
        ])
    );

    fs::remove_file(wiki.path().join("new.md")).unwrap();
    let one = "the server changed files the service leaves out, so the folder cannot take its \
               changes until they are fixed: page-3.md (a page over 1 MiB)";
    assert_eq!(
        wiki.tick(),
        Outcome::Blocked(one.to_owned()),
        "a fixed file is no longer named"
    );
    wiki.write("page-3.md", "# Page 3\n\nfixed here\n");
    assert_eq!(wiki.tick(), Outcome::Pushed);
    assert_eq!(wiki.server_head(), wiki.head());
    assert!(wiki.ok(&["show", "HEAD:page-3.md"]).contains("fixed here"));
    assert!(wiki.ok(&["show", "HEAD:new.md"]).contains("from elsewhere"));
    assert!(wiki.ok(&["log", "-1", "--format=%B"]).contains("Overlap: "));
    assert_eq!(wiki.tick(), Outcome::Idle);
    assert_eq!(wiki.reported()["skipped"], json!([]));
}

/// A page that turns into a symlink after the service looked at it and before it staged it is
/// left out all the same.
#[test]
fn a_file_that_turns_bad_as_it_is_staged_is_left_out() {
    let mut wiki = Wiki::racing();
    wiki.write("page-2.md", "# Page 2\n\na good edit\n");
    wiki.write("new.md", "# New\n\na page for now\n");
    wiki.run_before(
        "update-index --add",
        "cd \"$GIT_WORK_TREE\" && rm new.md && ln -s page-1.md new.md\n",
    );
    assert_eq!(wiki.tick(), Outcome::Pushed);
    assert!(wiki.raced(), "the file turned into a symlink");
    assert!(!wiki.has("HEAD", "new.md"));
    assert!(wiki.ok(&["show", "HEAD:page-2.md"]).contains("a good edit"));
    assert_eq!(wiki.ok(&["ls-files", "new.md"]), "", "it is not staged");
    assert_eq!(
        wiki.reported()["skipped"],
        skipped(&[("new.md".to_owned(), "a symlink")])
    );
}

/// Staging a symlink by hand does not get it into the service's commit.
#[test]
fn a_bad_file_staged_by_hand_is_left_out_of_the_save() {
    let mut wiki = Wiki::new();
    std::os::unix::fs::symlink("page-1.md", wiki.path().join("link.md")).unwrap();
    wiki.ok(&["add", "link.md"]);
    wiki.write("page-2.md", "# Page 2\n\na good edit\n");
    assert_eq!(wiki.tick(), Outcome::Pushed);
    assert!(!wiki.has("HEAD", "link.md"));
    assert_eq!(wiki.ok(&["ls-files", "link.md"]), "", "it is not staged");
    assert!(wiki.ok(&["show", "HEAD:page-2.md"]).contains("a good edit"));
    assert_eq!(
        wiki.reported()["skipped"],
        skipped(&[("link.md".to_owned(), "a symlink")])
    );
}

/// The report lists at most twenty left-out paths, and fewer when they are long, so that it
/// fits in its header, and counts the rest.
#[test]
fn the_report_lists_twenty_skipped_paths_and_counts_the_rest() {
    let mut wiki = Wiki::new();
    for n in 0..25 {
        let link = wiki.path().join(format!("link-{n:02}.md"));
        std::os::unix::fs::symlink("page-1.md", link).unwrap();
    }
    assert_eq!(wiki.tick(), Outcome::Idle);
    let reported = wiki.reported();
    let listed = reported["skipped"].as_array().unwrap();
    assert_eq!(listed.len(), 20);
    assert_eq!(listed[0]["path"], "link-00.md");
    assert_eq!(reported["skipped_more"], 5);

    let mut wiki = Wiki::new();
    let deep = vec!["d".repeat(250); 6].join("/");
    for n in 0..3 {
        let link = wiki.path().join(format!("{deep}/link-{n}.md"));
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink("page-1.md", link).unwrap();
    }
    assert_eq!(wiki.tick(), Outcome::Idle);
    let reported = wiki.reported();
    let listed = reported["skipped"].as_array().unwrap().len();
    assert!(listed < 3, "{listed} long paths fit");
    assert_eq!(reported["skipped_more"], 3 - listed);
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
