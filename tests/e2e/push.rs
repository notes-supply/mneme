use std::io::{Read, Write};
use std::net::TcpStream;
use std::os::unix::fs::PermissionsExt;

use flate2::Compression;
use flate2::write::GzEncoder;
use serde_json::Value;

use crate::git::{self, Isolation, Work};
use crate::history::{assert_versions_match_git, events, live_page};
use crate::server::{AUTHORIZATION, Server, TOKEN};

fn bearer() -> String {
    format!("Bearer {TOKEN}")
}

fn head(server: &Server) -> Value {
    server.json("/v1/status")["head"].clone()
}

/// A server with one pushed commit, and the clone that pushed it.
fn wiki() -> (Server, Work) {
    let server = Server::empty(&Isolation::new());
    let work = Work::clone_from(&server.git_url(), AUTHORIZATION);
    work.write("index.md", "# Home\n\nThe start.\n");
    work.commit("Start");
    let (ok, said) = work.push(&["origin", "main"]);
    assert!(ok, "{said}");
    (server, work)
}

/// Pushes what `work` has on main and checks that the server refused it with `reason`.
fn assert_refused(server: &Server, work: &Work, args: &[&str], reason: &str) {
    let before = head(server);
    let (ok, said) = work.push(args);
    assert!(!ok, "the push of {args:?} was accepted");
    assert!(said.contains(reason), "{reason:?} not in: {said}");
    assert_eq!(head(server), before, "main did not move");
}

#[test]
fn clone_commit_push_and_read_the_change() {
    let (server, work) = wiki();
    let repo = server.repo();
    let config = |key: &str| work.git.ok(repo, &["--git-dir", ".", "config", key]);
    assert_eq!(config("receive.denyNonFastForwards").trim(), "true");
    assert_eq!(config("receive.denyDeletes").trim(), "true");
    assert_eq!(config("receive.fsckObjects").trim(), "true");
    assert_eq!(config("http.receivepack").trim(), "true");
    assert_eq!(config("core.bigFileThreshold").trim(), "1048576");
    assert_eq!(config("receive.maxInputSize").trim(), "1073741824");
    let hook = std::fs::metadata(repo.join("hooks/pre-receive")).unwrap();
    assert_eq!(hook.permissions().mode() & 0o111, 0o111);

    work.write("index.md", "# Home\n\nThe start, and more.\n");
    let commit = work.commit("Say more");
    let (ok, said) = work.push(&["origin", "main"]);
    assert!(ok, "{said}");
    assert_eq!(head(&server), commit.as_str());
    let home = live_page(&server, "index.md");
    assert_eq!(home["versions"], 2);
    let page = server.json(&format!("/v1/pages/{}", home["id"].as_str().unwrap()));
    assert_eq!(page["markdown"], "# Home\n\nThe start, and more.\n");
    assert_versions_match_git(&server, &work);

    let log = server.log();
    let pushes: Vec<Value> = log
        .iter()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|line| line["event"] == "push")
        .collect();
    assert_eq!(pushes.len(), 2);
    assert_eq!(pushes[1]["machine"], "laptop");
    assert_eq!(pushes[1]["commits"], 1);
    assert!(
        log.iter()
            .any(|line| line.contains(r#""machine":"laptop""#)
                && line.contains("git-receive-pack"))
    );
    assert!(!log.join("\n").contains(TOKEN), "the token is never logged");
}

#[test]
fn a_pushed_history_indexes_as_a_fresh_build_does() {
    let git = Isolation::new();
    let server = Server::empty(&git);
    let work = git::standard();
    work.ok(&["remote", "add", "origin", &server.git_url()]);
    work.ok(&["config", "http.extraHeader", AUTHORIZATION]);
    let (ok, said) = work.push(&["origin", "main~5:refs/heads/main"]);
    assert!(ok, "{said}");
    let (ok, said) = work.push(&["origin", "main"]);
    assert!(ok, "{said}");
    assert_versions_match_git(&server, &work);
    let fresh = server.again(&git);
    for path in [
        "/v1/pages?deleted=true",
        "/v1/changes?limit=500",
        "/v1/files",
        "/v1/changes/files",
    ] {
        assert_eq!(server.get(path).body, fresh.get(path).body, "{path}");
    }
    let alpha = live_page(&server, "greek/alpha.md");
    assert_eq!(events(&server, &alpha).len(), 4);
    let readme = server.json("/v1/files/notes/readme.txt");
    assert_eq!(readme["file"]["versions"], 2, "one version in each push");
}

#[test]
fn files_pushed_one_commit_at_a_time_index_as_a_fresh_build_does() {
    let git = Isolation::new();
    let server = Server::empty(&git);
    let work = crate::files::brand();
    work.ok(&["remote", "add", "origin", &server.git_url()]);
    work.ok(&["config", "http.extraHeader", AUTHORIZATION]);
    for behind in (0..5).rev() {
        let (ok, said) = work.push(&["origin", &format!("main~{behind}:refs/heads/main")]);
        assert!(ok, "{said}");
    }
    let fresh = server.again(&git);
    let of_clone = Server::of(&work);
    for path in [
        "/v1/files",
        "/v1/changes/files",
        "/v1/files/brand/archive/Kit%202026.zip",
        "/v1/files/notes/readme.txt",
    ] {
        let pushed = server.get(path).body;
        assert_eq!(pushed, fresh.get(path).body, "{path}");
        assert_eq!(pushed, of_clone.get(path).body, "{path}");
    }
}

#[test]
fn a_ref_other_than_main_is_refused() {
    let (server, work) = wiki();
    assert_refused(
        &server,
        &work,
        &["origin", "HEAD:refs/heads/other"],
        "mneme: only refs/heads/main can be pushed, not refs/heads/other",
    );
    work.ok(&["tag", "v1"]);
    assert_refused(&server, &work, &["origin", "v1"], "not refs/tags/v1");
}

#[test]
fn a_merge_commit_is_refused() {
    let (server, work) = wiki();
    work.ok(&["switch", "-q", "-c", "side"]);
    work.write("side.md", "# Side\n");
    work.commit("Side");
    work.ok(&["switch", "-q", "main"]);
    work.write("main.md", "# Main\n");
    work.commit("Main");
    work.ok(&["merge", "-q", "--no-ff", "-m", "Merge side", "side"]);
    let merge = work.ok(&["rev-parse", "HEAD"]);
    assert_refused(
        &server,
        &work,
        &["origin", "main"],
        &format!("commit {} is a merge", merge.trim()),
    );
}

#[test]
fn a_symlink_is_refused() {
    let (server, work) = wiki();
    std::os::unix::fs::symlink("index.md", work.file("link.md")).unwrap();
    work.commit("Link");
    assert_refused(
        &server,
        &work,
        &["origin", "main"],
        "has a symlink at link.md",
    );
}

#[test]
fn a_submodule_is_refused() {
    let (server, work) = wiki();
    let target = work.ok(&["rev-parse", "HEAD"]);
    let entry = format!("160000,{},vendor/lib", target.trim());
    work.ok(&["update-index", "--add", "--cacheinfo", &entry]);
    work.ok(&["commit", "-q", "-m", "Add a submodule"]);
    assert_refused(
        &server,
        &work,
        &["origin", "main"],
        "has a submodule or nested repository at vendor/lib",
    );
}

/// Makes `main` a commit whose root tree has one entry, a folder named `name` holding a page.
/// Stock `git mktree` writes such a tree whatever the name is.
fn commit_with_folder_named(work: &Work, name: &str) {
    let blob = work.ok(&["hash-object", "-w", "index.md"]);
    let inner = work.git_stdin(
        &["mktree", "-z"],
        &format!("100644 blob {}\tconfig\0", blob.trim()),
    );
    let outer = work.git_stdin(
        &["mktree", "-z"],
        &format!("040000 tree {}\t{name}\0", inner.trim()),
    );
    let commit = work.ok(&[
        "commit-tree",
        outer.trim(),
        "-p",
        "HEAD",
        "-m",
        &format!("Hide a folder named {name}"),
    ]);
    work.ok(&["update-ref", "refs/heads/main", commit.trim()]);
}

#[test]
fn a_path_named_dot_git_is_refused() {
    for name in [".git", ".GIT", "git~1"] {
        let (server, work) = wiki();
        commit_with_folder_named(&work, name);
        assert_refused(&server, &work, &["origin", "main"], "hasDotgit");
    }
}

#[test]
fn a_tree_with_a_dot_or_dotdot_entry_is_refused() {
    for (name, reason) in [("..", "hasDotdot"), (".", "hasDot:")] {
        let (server, work) = wiki();
        commit_with_folder_named(&work, name);
        assert_refused(&server, &work, &["origin", "main"], reason);
    }
}

#[test]
fn a_page_over_one_mebibyte_is_refused() {
    let (server, work) = wiki();
    work.write("big.md", &"a".repeat((1 << 20) + 1));
    work.write("big.txt", &"a".repeat((1 << 20) + 1));
    work.commit("Big");
    assert_refused(
        &server,
        &work,
        &["origin", "main"],
        "has a page over 1 MiB at big.md",
    );
    work.ok(&["rm", "-q", "--cached", "big.md"]);
    work.ok(&["commit", "-q", "--amend", "-m", "Big, but not a page"]);
    let (ok, said) = work.push(&["origin", "main"]);
    assert!(ok, "a large file that is not a page is stored: {said}");
}

#[test]
fn a_page_over_one_mebibyte_with_a_name_git_quotes_is_refused() {
    let (server, work) = wiki();
    work.write("minnen/café.md", &"a".repeat((1 << 20) + 1));
    work.commit("Big café");
    assert_refused(
        &server,
        &work,
        &["origin", "main"],
        "has a page over 1 MiB at minnen/café.md",
    );
}

/// A file over 10 MiB whose bytes are the same block over and over, with `tail` at its end.
fn repetitive(tail: &str) -> Vec<u8> {
    let block = crate::files::bytes(8, 4096);
    let mut file: Vec<u8> = block
        .iter()
        .copied()
        .cycle()
        .take((10 << 20) + 4096)
        .collect();
    file.extend_from_slice(tail.as_bytes());
    file
}

/// Stock Git stores the second version of a large file as a delta of the first, and the server
/// keeps a pack of a hundred objects or more as it came. Reading such a file back would mean
/// rebuilding it in memory, so the push check refuses it and says what to set.
#[test]
fn a_large_file_pushed_as_a_delta_is_refused_until_git_is_told_to_store_it_whole() {
    let (server, work) = wiki();
    work.write_bytes("brand/film.bin", &repetitive("the first cut"));
    work.commit("Add a film");
    let (ok, said) = work.push(&["origin", "main"]);
    assert!(ok, "a first version has nothing to be a delta of: {said}");
    let second = repetitive("the second cut");
    work.write_bytes("brand/film.bin", &second);
    for page in 0..100 {
        work.write(&format!("notes/{page}.md"), &format!("# Note {page}\n"));
    }
    work.commit("Cut the film again, and write a hundred notes");
    assert_refused(
        &server,
        &work,
        &["origin", "main"],
        "brand/film.bin is over 10 MiB and the push stores it as a delta of another file",
    );
    assert_eq!(server.get("/healthz").body, "ok");

    work.ok(&["config", "core.bigFileThreshold", "1m"]);
    work.ok(&["repack", "-q", "-a", "-d", "-F"]);
    let (ok, said) = work.push(&["origin", "main"]);
    assert!(
        ok,
        "the same commit goes through once it is stored whole: {said}"
    );
    let raw = server.get("/v1/raw/brand/film.bin");
    assert_eq!(raw.status, 200);
    assert!(raw.bytes == second, "the second cut is read back");
    let film = server.json("/v1/files/brand/film.bin");
    assert_eq!(film["file"]["versions"], 2);
    assert_eq!(film["file"]["size"], second.len());
}

#[test]
fn a_force_push_is_refused() {
    let (server, work) = wiki();
    work.write("index.md", "# Home\n\nRewritten.\n");
    work.ok(&["commit", "-q", "-a", "--amend", "-m", "Rewrite"]);
    assert_refused(
        &server,
        &work,
        &["--force", "origin", "main"],
        "non-fast-forward",
    );
}

#[test]
fn deleting_main_is_refused() {
    let (server, work) = wiki();
    assert_refused(
        &server,
        &work,
        &["origin", ":main"],
        "refs/heads/main cannot be deleted",
    );
}

#[test]
fn git_requests_without_the_right_token_are_401_with_no_detail() {
    let (server, _work) = wiki();
    let path = "/git/wiki.git/info/refs?service=git-upload-pack";
    let wrong = "Bearer not-the-token".to_owned();
    let basic = "Basic YmxhY2tib3g6dG9rZW4=".to_owned();
    for authorization in [None, Some(wrong), Some(basic)] {
        let headers: Vec<(&str, &str)> = authorization
            .iter()
            .map(|value| ("authorization", value.as_str()))
            .collect();
        let reply = server.get_with(path, &headers);
        assert_eq!(reply.status, 401, "{authorization:?}");
        assert_eq!(reply.body, "", "{authorization:?}");
    }
    let output = Isolation::new().git(
        std::env::temp_dir().as_path(),
        &[
            "-c",
            "http.extraHeader=Authorization: Bearer not-the-token",
            "ls-remote",
            &server.git_url(),
        ],
    );
    assert!(!output.status.success());
    assert!(!server.log().join("\n").contains("not-the-token"));
}

#[test]
fn protocol_version_two_is_passed_to_git() {
    let (server, _work) = wiki();
    let reply = server.get_with(
        "/git/wiki.git/info/refs?service=git-upload-pack",
        &[("authorization", &bearer()), ("git-protocol", "version=2")],
    );
    assert_eq!(reply.status, 200);
    assert!(reply.body.contains("version 2"), "{}", reply.body);
}

#[test]
fn a_gzip_request_body_is_read() {
    let (server, work) = wiki();
    let request = b"0014command=ls-refs\n00010000";
    let mut gzip = GzEncoder::new(Vec::new(), Compression::default());
    gzip.write_all(request).unwrap();
    let body = gzip.finish().unwrap();
    let reply = server.post(
        "/git/wiki.git/git-upload-pack",
        &[
            ("authorization", &bearer()),
            ("git-protocol", "version=2"),
            ("content-type", "application/x-git-upload-pack-request"),
            ("content-encoding", "gzip"),
        ],
        &body,
    );
    assert_eq!(reply.status, 200);
    let head = work.ok(&["rev-parse", "HEAD"]);
    assert!(
        reply
            .body
            .contains(&format!("{} refs/heads/main", head.trim())),
        "{}",
        reply.body
    );
}

#[test]
fn a_body_that_declares_more_than_the_largest_pack_is_refused_before_git_runs() {
    let (server, _work) = wiki();
    let addr = server.url.trim_start_matches("http://");
    let mut stream = TcpStream::connect(addr).unwrap();
    write!(
        stream,
        "POST /git/wiki.git/git-receive-pack HTTP/1.1\r\nHost: {addr}\r\n\
         Authorization: Bearer {TOKEN}\r\nContent-Length: {}\r\n\
         Content-Type: application/x-git-receive-pack-request\r\n\r\n",
        (1_u64 << 30) + (1 << 20) + 1
    )
    .unwrap();
    let mut status = [0; 12];
    stream.read_exact(&mut status).unwrap();
    assert_eq!(&status, b"HTTP/1.1 413");
}

/// The server's own limit is 1 GiB, which no test sends. Git keeps it, so it is lowered here
/// in the repository, and the pack is of bytes that do not compress, larger than what the
/// connection holds unread, so that the client is still sending when Git gives up.
#[test]
fn a_pack_over_the_limit_is_refused_by_git_with_a_reason_and_the_server_stays_healthy() {
    let (server, work) = wiki();
    work.git.ok(
        server.repo(),
        &["--git-dir", ".", "config", "receive.maxInputSize", "100000"],
    );
    work.write_bytes("big.bin", &crate::files::bytes(9, 24 << 20));
    work.commit("Add a large file");
    assert_refused(
        &server,
        &work,
        &["origin", "main"],
        "remote: fatal: pack exceeds maximum allowed size",
    );
    let (_, said) = work.push(&["origin", "main"]);
    assert!(
        said.contains("[remote rejected]") && !said.contains("hung up"),
        "the push is refused, not cut off: {said}"
    );

    assert_eq!(server.get("/healthz").body, "ok");
    work.ok(&["reset", "-q", "--hard", "HEAD~"]);
    work.write("index.md", "# Home\n\nAfter the large push.\n");
    let commit = work.commit("Say more");
    let (ok, said) = work.push(&["origin", "main"]);
    assert!(ok, "{said}");
    assert_eq!(head(&server), commit.as_str());
}

#[test]
fn sigterm_stops_the_server() {
    let (server, _work) = wiki();
    assert_eq!(server.get("/healthz").body, "ok");
    let (status, log) = server.terminate();
    assert!(status.success(), "{status}");
    assert!(
        log.last().unwrap().contains(r#""event":"stopped""#),
        "{log:?}"
    );
}
