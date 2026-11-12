//! A service that follows: it keeps a copy of the server's `main` with a read-only token, takes
//! every change that nothing in the folder is in the way of, and never stages, commits or
//! pushes. Then the same with a Git host over HTTPS and a token that may only read.

use std::fs::{self, File};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use mneme::autosave::{Autosave, Outcome, Remote, Settings, Token};
use serde_json::Value;
use tempfile::TempDir;

use crate::git::Isolation;
use crate::remote::{Backend, Service, bare_wiki, mneme};
use crate::server::{AUTHORIZATION, Options, READER, Server};

const FETCH_EVERY: Duration = Duration::from_secs(30);
/// The fake clock starts at 2027-01-15T08:00:00Z.
const START: u64 = 1_800_000_000;

/// A server with three pages and a folder whose service follows it with [`READER`]'s token.
struct Mirror {
    server: Server,
    git: Isolation,
    dir: TempDir,
    service: Autosave,
    now: SystemTime,
}

impl Mirror {
    fn new() -> Self {
        let git = Isolation::new();
        let options = Options {
            reader: true,
            ..Options::default()
        };
        let server = Server::empty_with(&git, &options);
        for page in 1..=3 {
            let text = format!("# Page {page}\n");
            push_from_elsewhere(&git, &server, &format!("page-{page}.md"), &text);
        }
        let dir = TempDir::new().unwrap();
        let header = format!("http.extraHeader=Authorization: Bearer {}", READER.1);
        let url = server.git_url();
        git.ok(dir.path(), &["-c", &header, "clone", "-q", &url, "."]);
        let service = Autosave::new(Settings {
            folder: dir.path().to_owned(),
            remote: Remote::Server {
                url: server.url.clone(),
                token: Token(READER.1.to_owned()),
            },
            machine: READER.0.to_owned(),
            quiet: Duration::from_secs(15),
            fetch_every: FETCH_EVERY,
            report_every: Duration::from_secs(300),
            stale_lock: Duration::from_secs(300),
            git: "git".into(),
            follow: true,
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

    /// A tick once a fetch is due.
    fn tick(&mut self) -> Outcome {
        self.now += FETCH_EVERY + Duration::from_secs(1);
        self.service.tick(self.now)
    }

    /// A tick a second after the last, which fetches nothing.
    fn tick_soon(&mut self) -> Outcome {
        self.now += Duration::from_secs(1);
        self.service.tick(self.now)
    }

    /// Writes `text` to `path` in the folder, dated long ago, so that a writer's service would
    /// save it at once.
    fn write(&self, path: &str, text: &str) {
        let file = self.path().join(path);
        fs::write(&file, text).unwrap();
        File::options()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(UNIX_EPOCH)
            .unwrap();
    }

    fn read(&self, path: &str) -> String {
        fs::read_to_string(self.path().join(path)).unwrap()
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

    fn push_from_elsewhere(&self, path: &str, text: &str) {
        push_from_elsewhere(&self.git, &self.server, path, text);
    }

    /// What `/v1/machines` says of the mirror.
    fn reported(&self) -> Value {
        self.server.json("/v1/machines")["machines"]
            .as_array()
            .unwrap()
            .iter()
            .find(|machine| machine["name"] == READER.0)
            .unwrap()
            .clone()
    }

    /// Whether the folder's `main` ever moved but to follow the server.
    fn only_followed(&self) -> bool {
        self.ok(&["reflog", "--format=%gs", "main"])
            .lines()
            .all(|line| line.starts_with("clone") || line.starts_with("autosave: move"))
    }
}

/// Commits `text` to `path` on a writer's clone and pushes it.
fn push_from_elsewhere(git: &Isolation, server: &Server, path: &str, text: &str) {
    let other = TempDir::new().unwrap();
    let header = format!("http.extraHeader={AUTHORIZATION}");
    let url = server.git_url();
    git.ok(other.path(), &["-c", &header, "clone", "-q", &url, "."]);
    fs::write(other.path().join(path), text).unwrap();
    git.ok(other.path(), &["add", "-A"]);
    git.ok(other.path(), &["commit", "-q", "-m", "Edit elsewhere"]);
    git.ok(
        other.path(),
        &["-c", &header, "push", "-q", "origin", "main"],
    );
}

fn blocked(outcome: &Outcome) -> &str {
    match outcome {
        Outcome::Blocked(reason) => reason,
        other => panic!("not blocked: {other:?}"),
    }
}

#[test]
fn a_follower_takes_the_servers_changes_and_never_saves_or_pushes() {
    let mut mirror = Mirror::new();
    assert_eq!(mirror.tick(), Outcome::Idle);
    mirror.push_from_elsewhere("page-1.md", "# Page 1\n\nFrom a writer.\n");
    assert_eq!(mirror.tick(), Outcome::Pulled);
    assert_eq!(mirror.head(), mirror.server_head());
    assert_eq!(mirror.read("page-1.md"), "# Page 1\n\nFrom a writer.\n");
    assert_eq!(mirror.tick(), Outcome::Idle);
    let reported = mirror.reported();
    assert_eq!(reported["state"], "idle");
    assert_eq!(reported["current"], true);

    mirror.write("page-2.md", "# Page 2\n\nEdited in the mirror.\n");
    mirror.write("new.md", "# New\n");
    let before = mirror.server_head();
    assert_eq!(mirror.tick(), Outcome::Idle, "an edit no change needs");
    mirror.push_from_elsewhere("page-3.md", "# Page 3\n\nFrom a writer.\n");
    assert_eq!(mirror.tick(), Outcome::Pulled);
    assert_eq!(mirror.read("page-3.md"), "# Page 3\n\nFrom a writer.\n");
    assert_eq!(
        mirror.read("page-2.md"),
        "# Page 2\n\nEdited in the mirror.\n",
        "the edit stays in the folder"
    );
    assert_ne!(mirror.server_head(), before);
    assert_eq!(mirror.head(), mirror.server_head(), "nothing was pushed");
    assert_eq!(
        mirror.ok(&["status", "--porcelain", "--untracked-files=all"]),
        " M page-2.md\n?? new.md\n",
        "nothing was staged"
    );
    assert!(mirror.only_followed());
    let pushes = mirror
        .server
        .log()
        .iter()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| {
            line["machine"] == READER.0 && line["path"] == "/git/wiki.git/git-receive-pack"
        })
        .count();
    assert_eq!(pushes, 0);

    mirror.ok(&[
        "config",
        "remote.origin.url",
        "http://127.0.0.1:9/git/wiki.git",
    ]);
    assert!(matches!(mirror.tick(), Outcome::Offline(_)));
}

#[test]
fn a_follower_is_blocked_by_a_change_in_the_way_and_names_it_until_it_goes() {
    let mut mirror = Mirror::new();
    mirror.write("page-1.md", "# Page 1\n\nEdited in the mirror.\n");
    mirror.push_from_elsewhere("page-1.md", "# Page 1\n\nFrom a writer.\n");
    let reason = blocked(&mirror.tick()).to_owned();
    assert!(
        reason.contains("in the way") && reason.contains("page-1.md"),
        "{reason}"
    );
    assert_eq!(
        blocked(&mirror.tick_soon()),
        reason,
        "a tick that does not fetch says the same"
    );
    assert_eq!(
        mirror.read("page-1.md"),
        "# Page 1\n\nEdited in the mirror.\n"
    );
    assert_eq!(mirror.reported()["state"], "blocked");
    mirror.ok(&["checkout", "--", "page-1.md"]);
    assert_eq!(
        mirror.tick_soon(),
        Outcome::Pulled,
        "it moves once the edit is gone"
    );
    assert_eq!(mirror.read("page-1.md"), "# Page 1\n\nFrom a writer.\n");

    mirror.write("added.md", "# Mine\n");
    mirror.push_from_elsewhere("added.md", "# Theirs\n");
    let reason = blocked(&mirror.tick()).to_owned();
    assert!(reason.contains("added.md"), "{reason}");
    fs::remove_file(mirror.path().join("added.md")).unwrap();
    assert_eq!(mirror.tick_soon(), Outcome::Pulled);
    assert_eq!(mirror.read("added.md"), "# Theirs\n");
    assert_eq!(mirror.head(), mirror.server_head());
}

#[test]
fn a_follower_with_a_local_commit_is_blocked_and_pushes_nothing() {
    let mut mirror = Mirror::new();
    mirror.write("page-2.md", "# Page 2\n\nCommitted by hand.\n");
    mirror.ok(&["commit", "-q", "-am", "By hand"]);
    let server = mirror.server_head();
    let reason = blocked(&mirror.tick()).to_owned();
    assert!(
        reason.contains("follows") && reason.contains("local commits"),
        "{reason}"
    );
    mirror.push_from_elsewhere("page-1.md", "# Page 1\n\nFrom a writer.\n");
    assert_eq!(blocked(&mirror.tick()), reason, "diverged, it still waits");
    assert_ne!(mirror.server_head(), server);
    assert!(
        !mirror
            .git
            .ok(mirror.server.repo(), &["log", "--format=%s", "main"])
            .contains("By hand"),
        "the local commit never reached the server"
    );
}

#[test]
fn a_follower_of_a_git_host_with_a_token_that_may_only_read_takes_the_remotes_changes() {
    let git = Isolation::new();
    let root = TempDir::new().unwrap();
    let bare = bare_wiki(&git, root.path());
    let token = "read-token-of-mirror";
    let token_file = root.path().join("token");
    fs::write(&token_file, format!("{token}\n")).unwrap();
    let expected = format!("Basic {}", STANDARD.encode(format!("git:{token}")));
    let backend = Backend::read_only(&git, root.path(), expected);
    let url = format!("{}/wiki.git", backend.url);
    let folder = root.path().join("mirror");
    let args = |command: &str| -> Vec<String> {
        let mut args: Vec<String> = ["autosave", command, "--follow", "--folder"]
            .map(str::to_owned)
            .into();
        args.push(folder.to_str().unwrap().to_owned());
        args.extend([
            "--remote".to_owned(),
            url.clone(),
            "--token-file".to_owned(),
        ]);
        args.push(token_file.to_str().unwrap().to_owned());
        args.extend(["--name".to_owned(), "mirror".to_owned()]);
        args
    };
    let bin = root.path().join("bin");
    let init = mneme(&git, &bin).args(args("init")).output().unwrap();
    assert!(init.status.success(), "{init:?}");

    let writer = TempDir::new().unwrap();
    git.ok(writer.path(), &["clone", "-q", bare.to_str().unwrap(), "."]);
    fs::write(
        writer.path().join("notes.md"),
        "# Notes\n\nFrom a writer.\n",
    )
    .unwrap();
    git.ok(writer.path(), &["commit", "-q", "-am", "Write notes"]);
    git.ok(writer.path(), &["push", "-q", "origin", "main"]);
    let pushed = git.ok(writer.path(), &["rev-parse", "HEAD"]);
    fs::write(folder.join("index.md"), "# Home\n\nEdited in the mirror.\n").unwrap();

    let mut run = mneme(&git, &bin);
    run.args(args("run")).args([
        "--tick-seconds",
        "0.1",
        "--quiet-seconds",
        "0",
        "--fetch-seconds",
        "0.2",
    ]);
    let service = Service::start(run);
    service.wait_for("pulled");
    let log = service.terminate().join("\n");
    assert!(log.contains(r#""follow":true"#), "{log}");
    assert!(!log.contains(r#""outcome":"blocked""#), "{log}");
    assert_eq!(git.ok(&folder, &["rev-parse", "HEAD"]), pushed);
    assert_eq!(
        fs::read_to_string(folder.join("notes.md")).unwrap(),
        "# Notes\n\nFrom a writer.\n"
    );
    assert_eq!(
        fs::read_to_string(folder.join("index.md")).unwrap(),
        "# Home\n\nEdited in the mirror.\n",
        "the edit stays, unsaved"
    );
    assert_eq!(git.ok(&bare, &["rev-parse", "main"]), pushed);
    let targets = backend.targets.lock().unwrap().clone();
    assert!(
        targets
            .iter()
            .any(|target| target.contains("git-upload-pack")),
        "it fetched over HTTPS: {targets:?}"
    );
    assert!(
        !targets.iter().any(|target| target.contains("receive-pack")),
        "it never tried to push: {targets:?}"
    );
}
