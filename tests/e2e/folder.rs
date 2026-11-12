//! `mneme serve --folder`: the reading API of a clone on this machine, on loopback, following
//! the folder's `main` and writing nothing.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;
use tempfile::TempDir;

use crate::git;
use crate::server::Server;

/// A clone of the standard history in a folder of its own.
fn clone_standard() -> (git::Work, TempDir) {
    let work = git::standard();
    let folder = TempDir::new().unwrap();
    work.ok(&["clone", "-q", ".", folder.path().to_str().unwrap()]);
    (work, folder)
}

/// Every file under `dir`, with its length and when it was written.
fn snapshot(dir: &Path) -> BTreeMap<String, (u64, SystemTime)> {
    fn walk(dir: &Path, found: &mut BTreeMap<String, (u64, SystemTime)>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let metadata = entry.metadata().unwrap();
            if metadata.is_dir() {
                walk(&entry.path(), found);
            } else {
                found.insert(
                    entry.path().display().to_string(),
                    (metadata.len(), metadata.modified().unwrap()),
                );
            }
        }
    }
    let mut found = BTreeMap::new();
    walk(dir, &mut found);
    found
}

/// The status without the build time, which differs from one build to the next.
fn status(server: &Server) -> Value {
    let mut status = server.json("/v1/status");
    status.as_object_mut().unwrap().remove("build_ms");
    status
}

/// Waits until the server's head is `head`.
fn wait_for_head(server: &Server, head: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let status = status(server);
        if status["head"] == head {
            return status;
        }
        assert!(Instant::now() < deadline, "the head stayed at {status}");
        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_folder_answers_as_the_server_does_with_no_git_and_writes_nothing() {
    let (work, folder) = clone_standard();
    let before = snapshot(folder.path());
    let server = Server::folder(folder.path(), &work.git, &[]);
    let of_repo = Server::of(&work);
    for route in [
        "/v1/pages?deleted=true",
        "/v1/changes",
        "/v1/questions",
        "/v1/search?q=harbour",
        "/v1/paths/greek/alpha.md",
    ] {
        assert_eq!(server.json(route), of_repo.json(route), "{route}");
    }
    assert_eq!(status(&server), status(&of_repo));
    let id = server.json("/v1/pages")["pages"][0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        server.json(&format!("/v1/pages/{id}/versions")),
        of_repo.json(&format!("/v1/pages/{id}/versions"))
    );
    assert_eq!(
        server.json("/v1/machines"),
        serde_json::json!({ "total": 0, "machines": [] })
    );
    assert_eq!(server.get("/healthz").status, 200);
    let git = server.get("/git/wiki.git/info/refs?service=git-upload-pack");
    assert_eq!(git.status, 404, "no Git routes: {}", git.body);
    let (status, log) = server.terminate();
    assert!(status.success(), "{log:?}");
    assert_eq!(
        snapshot(folder.path()),
        before,
        "nothing was written to the folder or its .git"
    );
}

#[test]
fn a_folder_follows_new_commits_and_builds_again_after_a_reset() {
    let (work, folder) = clone_standard();
    let git = &work.git;
    let server = Server::folder(folder.path(), git, &[]);
    let pages = server.json("/v1/pages")["total"].as_u64().unwrap();

    fs::write(
        folder.path().join("new.md"),
        "# New\n\nA page after the start.\n",
    )
    .unwrap();
    git.ok(folder.path(), &["add", "new.md"]);
    git.ok(folder.path(), &["commit", "-q", "-m", "Add a page"]);
    let head = git.ok(folder.path(), &["rev-parse", "HEAD"]);
    let followed = wait_for_head(&server, head.trim());
    assert_eq!(followed["pages"].as_u64().unwrap(), pages + 1);
    assert!(
        server
            .log_with(r#""event":"followed""#)
            .iter()
            .any(|line| line.contains(r#""commits":1"#))
    );

    git.ok(folder.path(), &["reset", "-q", "--hard", "HEAD~3"]);
    fs::write(
        folder.path().join("other.md"),
        "# Other\n\nWritten after a reset.\n",
    )
    .unwrap();
    git.ok(folder.path(), &["add", "other.md"]);
    git.ok(
        folder.path(),
        &["commit", "-q", "-m", "Write after a reset"],
    );
    let head = git.ok(folder.path(), &["rev-parse", "HEAD"]);
    let rebuilt = wait_for_head(&server, head.trim());
    let commits = git.ok(folder.path(), &["rev-list", "--count", "HEAD"]);
    assert_eq!(
        rebuilt["commits"].as_u64().unwrap(),
        commits.trim().parse::<u64>().unwrap()
    );
    server.log_with(r#""event":"rebuilt""#);
    let found = server.get("/v1/paths/new.md");
    assert_eq!(
        found.status, 404,
        "the reset took the page away: {}",
        found.body
    );
    assert_eq!(server.json("/v1/paths/other.md")["current"], true);
}

#[test]
fn a_folder_answers_only_requests_that_name_a_loopback_host() {
    let (work, folder) = clone_standard();
    let server = Server::folder(folder.path(), &work.git, &[]);
    let port = server.url.rsplit(':').next().unwrap().to_owned();
    for host in [
        format!("localhost:{port}"),
        "LOCALHOST".to_owned(),
        format!("127.0.0.1:{port}"),
        format!("[::1]:{port}"),
    ] {
        assert_eq!(
            server.get_as(Some(&host), "/v1/status").status,
            200,
            "{host}"
        );
    }
    for host in [
        Some(format!("evil.example:{port}")),
        Some(format!("127.0.0.1.nip.io:{port}")),
        Some("localhost.evil.example".to_owned()),
        Some(format!("192.168.1.10:{port}")),
        None,
    ] {
        let reply = server.get_as(host.as_deref(), "/v1/pages");
        assert_eq!(reply.status, 421, "{host:?}: {}", reply.body);
        assert!(reply.json()["error"].is_string(), "{host:?}");
        assert_eq!(
            server.get_as(host.as_deref(), "/healthz").status,
            421,
            "{host:?}"
        );
    }
}

#[test]
fn a_folder_listens_on_loopback_only_and_says_which_port() {
    let (work, folder) = clone_standard();
    let server = Server::folder(folder.path(), &work.git, &[]);
    let listening: Value = server
        .log()
        .iter()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|line| line["event"] == "listening")
        .unwrap();
    let addr = listening["addr"].as_str().unwrap();
    let port: u16 = addr.strip_prefix("127.0.0.1:").unwrap().parse().unwrap();
    assert_ne!(port, 0, "port 0 gives the real port: {addr}");

    for addr in ["0.0.0.0:0", "[::]:0", "192.168.1.10:8081"] {
        let output = work
            .git
            .apply(
                Command::new(env!("CARGO_BIN_EXE_mneme"))
                    .args(["serve", "--folder"])
                    .arg(folder.path())
                    .args(["--addr", addr]),
            )
            .output()
            .unwrap();
        let said = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(1), "{addr}: {said}");
        assert!(said.contains("loopback"), "{addr}: {said}");
        assert!(!said.contains("listening"), "{addr}: {said}");
    }
    for conflict in [
        &["--repo", "/srv/wiki.git"][..],
        &["--tokens", "tokens.json"][..],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_mneme"))
            .args(["serve", "--folder"])
            .arg(folder.path())
            .args(conflict)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{conflict:?}: {output:?}");
    }
}

#[test]
fn a_folder_server_with_exit_with_parent_stops_when_its_parent_exits() {
    let (_work, folder) = clone_standard();
    let dir = TempDir::new().unwrap();
    let log = dir.path().join("serve.log");
    let script = format!(
        "'{}' serve --folder '{}' --addr 127.0.0.1:0 --exit-with-parent \
         </dev/null >/dev/null 2>'{}' &\nread go\nexit 0\n",
        env!("CARGO_BIN_EXE_mneme"),
        folder.path().display(),
        log.display(),
    );
    let mut parent = Command::new("sh")
        .args(["-c", &script])
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    let read = || fs::read_to_string(&log).unwrap_or_default();
    let deadline = Instant::now() + Duration::from_secs(20);
    while !read().contains(r#""event":"listening""#) {
        assert!(
            Instant::now() < deadline,
            "the server did not start: {}",
            read()
        );
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!read().contains("parent exited"), "{}", read());
    drop(parent.stdin.take());
    assert!(parent.wait().unwrap().success());
    let deadline = Instant::now() + Duration::from_secs(20);
    while !read().contains(r#""event":"stopped""#) {
        assert!(
            Instant::now() < deadline,
            "the server kept running: {}",
            read()
        );
        thread::sleep(Duration::from_millis(50));
    }
    assert!(read().contains(r#""event":"parent exited""#), "{}", read());
}
