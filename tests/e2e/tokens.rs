//! What a machine's token lets it do on the Git routes, and where the server reads the tokens
//! from.

use std::process::Command;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::git::{Isolation, Work};
use crate::server::{AUTHORIZATION, Options, READER, Server, TOKEN};

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

fn machine(server: &Server, name: &str) -> Value {
    server.json("/v1/machines")["machines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|machine| machine["name"] == name)
        .unwrap()
        .clone()
}

/// A server that knows [`READER`], with one page pushed by a writer, and that page's commit.
fn wiki(options: &Options) -> (Server, String) {
    let server = Server::empty_with(&Isolation::new(), options);
    let writer = Work::clone_from(&server.git_url(), AUTHORIZATION);
    writer.write("index.md", "# Home\n");
    let head = writer.commit("Start");
    let (ok, said) = writer.push(&["origin", "main"]);
    assert!(ok, "{said}");
    (server, head)
}

#[test]
fn a_read_only_token_fetches_and_reports_and_a_push_is_refused_with_a_reason_git_shows() {
    let options = Options {
        reader: true,
        ..Options::default()
    };
    let (server, head) = wiki(&options);
    let mirror = Work::clone_from(
        &server.git_url(),
        &format!("Authorization: {}", bearer(READER.1)),
    );
    assert_eq!(mirror.ok(&["rev-parse", "HEAD"]).trim(), head);
    mirror.ok(&["fetch", "-q", "origin"]);

    let report = json!({
        "state": "idle",
        "detail": "",
        "head": head,
        "skipped": [],
        "skipped_more": 0,
    });
    let reply = server.get_with(
        "/git/wiki.git/info/refs?service=git-upload-pack",
        &[
            ("authorization", &bearer(READER.1)),
            ("mneme-report", &STANDARD.encode(report.to_string())),
        ],
    );
    assert_eq!(reply.status, 200, "{}", reply.body);
    let listed = machine(&server, READER.0);
    assert_eq!(listed["state"], "idle");
    assert_eq!(listed["current"], true);

    mirror.write("index.md", "# Home\n\nFrom the mirror.\n");
    mirror.commit("Try to push");
    let (ok, said) = mirror.push(&["origin", "main"]);
    assert!(!ok, "the push was accepted");
    assert!(
        said.contains("mneme: mirror has a read-only token, which may fetch but not push"),
        "Git shows the reason: {said}"
    );
    assert_eq!(server.json("/v1/status")["head"], head.as_str());

    for (method, path) in [
        ("GET", "/git/wiki.git/info/refs?service=git-receive-pack"),
        (
            "GET",
            "/git/wiki.git/info/refs?service=git-upload-pack&service=git-receive-pack",
        ),
        ("POST", "/git/wiki.git/git-receive-pack"),
    ] {
        let reply = server.request(method, path, &[("authorization", &bearer(READER.1))], b"");
        assert_eq!(reply.status, 403, "{method} {path}");
        assert!(
            reply
                .header("content-type")
                .is_some_and(|kind| kind.starts_with("text/plain")),
            "{method} {path}"
        );
        assert!(reply.body.contains("read-only token"), "{method} {path}");
    }
    let writer = server.get_with(
        "/git/wiki.git/info/refs?service=git-receive-pack",
        &[("authorization", &bearer(TOKEN))],
    );
    assert_eq!(writer.status, 200, "a writer's token may still push");
}

#[test]
fn the_tokens_can_come_from_an_environment_variable() {
    let options = Options {
        reader: true,
        tokens_env: true,
    };
    let (server, head) = wiki(&options);
    assert_eq!(server.json("/v1/machines")["total"], 4);
    let mirror = Work::clone_from(
        &server.git_url(),
        &format!("Authorization: {}", bearer(READER.1)),
    );
    assert_eq!(mirror.ok(&["rev-parse", "HEAD"]).trim(), head);
    mirror.write("index.md", "# Home\n\nFrom the mirror.\n");
    mirror.commit("Try to push");
    assert!(!mirror.push(&["origin", "main"]).0);
}

#[test]
fn serve_takes_exactly_one_of_tokens_and_tokens_env() {
    let dir = TempDir::new().unwrap();
    let file = dir.path().join("tokens.json");
    std::fs::write(&file, "{}").unwrap();
    let repo = dir.path().join("wiki.git");
    let serve = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_mneme"))
            .args(["serve", "--addr", "127.0.0.1:0", "--repo"])
            .arg(&repo)
            .args(args)
            .env_remove("MNEME_UNSET_TOKENS")
            .output()
            .unwrap()
    };
    let file = file.to_str().unwrap();
    for args in [
        &[][..],
        &["--tokens", file, "--tokens-env", "MNEME_UNSET_TOKENS"][..],
    ] {
        let output = serve(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {output:?}");
    }
    let output = serve(&["--tokens-env", "MNEME_UNSET_TOKENS"]);
    let said = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success(), "{said}");
    assert!(said.contains("MNEME_UNSET_TOKENS"), "{said}");
}
