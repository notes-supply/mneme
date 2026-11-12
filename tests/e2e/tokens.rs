//! What a machine's token lets it do on the Git routes and the reading API, and where the
//! server reads the tokens from.

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
        ..Options::default()
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

#[test]
fn with_a_private_api_every_v1_route_asks_for_a_token_of_either_access() {
    let options = Options {
        reader: true,
        private_api: true,
        ..Options::default()
    };
    let (server, head) = wiki(&options);
    let wrong = bearer("not-a-token");
    let basic = "Basic bWlycm9yOnRlc3QtdG9rZW4tb2YtbWlycm9y".to_owned();
    for path in [
        "/v1/status",
        "/v1/pages",
        "/v1/search?q=home",
        "/v1/machines",
        "/v1/nothing",
        "/elsewhere",
    ] {
        for authorization in [None, Some(&wrong), Some(&basic)] {
            let headers: Vec<(&str, &str)> = authorization
                .iter()
                .map(|value| ("authorization", value.as_str()))
                .collect();
            let reply = server.get_with(path, &headers);
            assert_eq!(reply.status, 401, "{path} {authorization:?}");
            assert_eq!(reply.header("www-authenticate"), Some("Bearer"), "{path}");
            assert_eq!(reply.body, "", "{path} {authorization:?}");
        }
    }
    for token in [TOKEN, READER.1] {
        let authorization = bearer(token);
        let reply = server.get_with("/v1/status", &[("authorization", &authorization)]);
        assert_eq!(reply.status, 200, "{}", reply.body);
        assert_eq!(reply.json()["head"], head.as_str());
        let missing = server.get_with("/v1/nothing", &[("authorization", &authorization)]);
        assert_eq!(missing.status, 404);
    }
    assert_eq!(
        server.get("/healthz").body,
        "ok",
        "the probe needs no token"
    );
    let read: Vec<Value> = server
        .log_with(r#""machine":"mirror""#)
        .iter()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line["path"] == "/v1/status" && line["status"] == 200)
        .collect();
    let machines: Vec<&Value> = read.iter().map(|line| &line["machine"]).collect();
    assert_eq!(machines, ["laptop", READER.0], "the log names who read");
}
