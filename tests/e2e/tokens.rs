//! What a machine's token lets it do on the Git routes and the reading API, where the server
//! reads the tokens from, and the machines an admin adds while it runs.

use std::process::Command;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::api::fits_example;
use crate::git::{Isolation, Work};
use crate::server::{
    ADMIN, AUTHORIZATION, Options, READER, Reply, Server, TOKEN, TokensFrom, mneme_hash,
};

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
    wiki_on(&Isolation::new(), options)
}

fn wiki_on(git: &Isolation, options: &Options) -> (Server, String) {
    let server = Server::empty_with(git, options);
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
        tokens_from: TokensFrom::Env,
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

/// Options for a server that knows [`READER`] and [`ADMIN`].
fn with_admin() -> Options {
    Options {
        reader: true,
        admin: true,
        ..Options::default()
    }
}

/// A request to manage machines, with `token`, and `body` as JSON when there is one.
fn manage(server: &Server, method: &str, path: &str, token: &str, body: Option<&Value>) -> Reply {
    let authorization = bearer(token);
    let mut headers = vec![("authorization", authorization.as_str())];
    if body.is_some() {
        headers.push(("content-type", "application/json"));
    }
    let body = body.map_or_default(Value::to_string);
    server.request(method, path, &headers, body.as_bytes())
}

/// Adds the machine `name`, whose token is `test-token-of-<name>`, with `access`, as the admin.
fn add(server: &Server, name: &str, access: &str) -> Reply {
    let body = json!({ "sha256": mneme_hash(&token_of(name)), "access": access });
    manage(
        server,
        "PUT",
        &format!("/v1/tokens/{name}"),
        ADMIN.1,
        Some(&body),
    )
}

fn token_of(name: &str) -> String {
    format!("test-token-of-{name}")
}

/// The status of a fetch's first request, `info/refs`, with `token`.
fn fetch_status(server: &Server, token: &str) -> u16 {
    let authorization = bearer(token);
    server
        .get_with(
            "/git/wiki.git/info/refs?service=git-upload-pack",
            &[("authorization", &authorization)],
        )
        .status
}

fn clone_with(server: &Server, token: &str) -> Work {
    Work::clone_from(
        &server.git_url(),
        &format!("Authorization: {}", bearer(token)),
    )
}

/// The first line the server logged as `event`.
fn logged(server: &Server, event: &str) -> Value {
    server
        .log_with(&format!(r#""event":"{event}""#))
        .iter()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|line| line["event"] == event)
        .unwrap()
}

fn listed_names(server: &Server, path: &str, key: &str) -> Vec<String> {
    let reply = manage(server, "GET", path, ADMIN.1, None);
    assert_eq!(reply.status, 200, "{}", reply.body);
    reply.json()[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["name"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn an_admin_adds_machines_that_then_fetch_or_push_as_their_access_says() {
    let (server, head) = wiki(&with_admin());
    let added = add(&server, "studio", "write");
    assert_eq!(added.status, 201, "{}", added.body);
    assert_eq!(added.json()["name"], "studio");
    assert_eq!(added.json()["access"], "write");
    assert_eq!(add(&server, "backup", "read").status, 201);

    let studio = clone_with(&server, &token_of("studio"));
    studio.write("index.md", "# Home\n\nFrom the studio.\n");
    let pushed = studio.commit("Edit from the studio");
    let (ok, said) = studio.push(&["origin", "main"]);
    assert!(ok, "{said}");
    let backup = clone_with(&server, &token_of("backup"));
    assert_eq!(backup.ok(&["rev-parse", "HEAD"]).trim(), pushed);
    assert_ne!(pushed, head);
    backup.write("index.md", "# Home\n\nFrom the backup.\n");
    backup.commit("Try to push");
    let (ok, said) = backup.push(&["origin", "main"]);
    assert!(!ok && said.contains("read-only token"), "{said}");

    let listing = manage(&server, "GET", "/v1/tokens", ADMIN.1, None);
    assert_eq!(listing.status, 200, "{}", listing.body);
    fits_example("tokens", &listing.json());
    assert_eq!(listing.json()["total"], 2);
    for name in ["studio", "backup"] {
        assert!(
            !listing.body.contains(&mneme_hash(&token_of(name))),
            "the listing holds no hash: {}",
            listing.body
        );
    }
    let machines = server.json("/v1/machines");
    let known: Vec<&str> = machines["machines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|machine| machine["name"].as_str().unwrap())
        .collect();
    assert!(
        known.contains(&"studio") && known.contains(&"backup"),
        "{known:?}"
    );

    let replaced = add(&server, "studio", "read");
    assert_eq!(replaced.status, 200, "{}", replaced.body);
    studio.write("index.md", "# Home\n\nAgain from the studio.\n");
    studio.commit("Edit again");
    assert!(
        !studio.push(&["origin", "main"]).0,
        "studio may only read now"
    );

    let admin = server.get_with(
        "/git/wiki.git/info/refs?service=git-receive-pack",
        &[("authorization", &bearer(ADMIN.1))],
    );
    assert_eq!(admin.status, 200, "an admin token may push too");

    let addition = logged(&server, "token added");
    assert_eq!(
        (&addition["by"], &addition["machine"]),
        (&json!("keeper"), &json!("studio"))
    );
    let log = server.log().join("\n");
    for secret in [token_of("studio"), mneme_hash(&token_of("studio"))] {
        assert!(!log.contains(&secret), "the log holds no token or hash");
    }
}

#[test]
fn a_removed_machine_is_refused_at_its_next_request() {
    let (server, _) = wiki(&with_admin());
    assert_eq!(add(&server, "studio", "write").status, 201);
    assert_eq!(fetch_status(&server, &token_of("studio")), 200);

    let removed = manage(&server, "DELETE", "/v1/tokens/studio", ADMIN.1, None);
    assert_eq!(removed.status, 204, "{}", removed.body);
    assert_eq!(fetch_status(&server, &token_of("studio")), 401);
    let again = manage(&server, "DELETE", "/v1/tokens/studio", ADMIN.1, None);
    assert_eq!(again.status, 404, "{}", again.body);
    assert_eq!(
        listed_names(&server, "/v1/tokens", "tokens"),
        Vec::<String>::new()
    );
    assert!(!listed_names(&server, "/v1/machines", "machines").contains(&"studio".to_owned()));

    let removal = logged(&server, "token removed");
    assert_eq!(
        (&removal["by"], &removal["machine"]),
        (&json!("keeper"), &json!("studio"))
    );
}

#[test]
fn only_an_admin_token_may_manage_machines() {
    let (server, _) = wiki(&with_admin());
    assert_eq!(add(&server, "studio", "write").status, 201);
    let body = json!({ "sha256": mneme_hash("test-token-of-intruder"), "access": "write" });
    for token in [TOKEN, READER.1, &token_of("studio")] {
        for (method, path, body) in [
            ("GET", "/v1/tokens", None),
            ("PUT", "/v1/tokens/intruder", Some(&body)),
            ("DELETE", "/v1/tokens/studio", None),
        ] {
            let reply = manage(&server, method, path, token, body);
            assert_eq!(
                reply.status, 403,
                "{method} {path} with {token}: {}",
                reply.body
            );
            assert!(reply.json()["error"].is_string());
        }
    }
    for authorization in [vec![], vec![("authorization", "Bearer not-a-token")]] {
        let reply = server.request("GET", "/v1/tokens", &authorization, b"");
        assert_eq!(reply.status, 401);
        assert_eq!(reply.body, "");
    }
    assert_eq!(listed_names(&server, "/v1/tokens", "tokens"), ["studio"]);
    assert_eq!(fetch_status(&server, "test-token-of-intruder"), 401);
}

#[test]
fn a_machine_added_at_runtime_cannot_take_a_name_or_a_hash_of_the_tokens_file() {
    let (server, _) = wiki(&with_admin());
    let hash = |token: &str| json!({ "sha256": mneme_hash(token), "access": "write" });
    for name in ["laptop", "keeper", "mirror"] {
        let path = format!("/v1/tokens/{name}");
        let reply = manage(
            &server,
            "PUT",
            &path,
            ADMIN.1,
            Some(&hash("test-token-new")),
        );
        assert_eq!(reply.status, 409, "{name}: {}", reply.body);
        let reply = manage(&server, "DELETE", &path, ADMIN.1, None);
        assert_eq!(reply.status, 409, "{name}: {}", reply.body);
    }
    let reply = manage(
        &server,
        "PUT",
        "/v1/tokens/studio",
        ADMIN.1,
        Some(&hash(TOKEN)),
    );
    assert_eq!(
        reply.status, 409,
        "a token of the tokens file: {}",
        reply.body
    );
    assert_eq!(fetch_status(&server, "test-token-new"), 401);
    let reply = server.get_with(
        "/git/wiki.git/info/refs?service=git-receive-pack",
        &[("authorization", &bearer(TOKEN))],
    );
    assert_eq!(reply.status, 200, "laptop's token is still laptop's");

    let fine = mneme_hash("test-token-of-studio");
    for (path, body) in [
        (
            "/v1/tokens/Laptop",
            json!({ "sha256": fine, "access": "write" }),
        ),
        (
            "/v1/tokens/two%20words",
            json!({ "sha256": fine, "access": "write" }),
        ),
        (
            "/v1/tokens/studio",
            json!({ "sha256": fine, "access": "admin" }),
        ),
        (
            "/v1/tokens/studio",
            json!({ "sha256": "abc", "access": "write" }),
        ),
        ("/v1/tokens/studio", json!({ "sha256": fine })),
        (
            "/v1/tokens/studio",
            json!({ "sha256": fine, "access": "read", "name": "x" }),
        ),
    ] {
        let reply = manage(&server, "PUT", path, ADMIN.1, Some(&body));
        assert_eq!(reply.status, 422, "{path} {body}: {}", reply.body);
    }
    assert_eq!(
        listed_names(&server, "/v1/tokens", "tokens"),
        Vec::<String>::new()
    );
}

#[test]
fn machines_added_at_runtime_survive_a_restart() {
    let git = Isolation::new();
    let (server, head) = wiki_on(&git, &with_admin());
    assert_eq!(add(&server, "studio", "write").status, 201);
    assert_eq!(add(&server, "backup", "read").status, 201);
    let before = manage(&server, "GET", "/v1/tokens", ADMIN.1, None).json();

    let (status, _, server) = server.restart(&git);
    assert!(status.success());
    let after = manage(&server, "GET", "/v1/tokens", ADMIN.1, None).json();
    assert_eq!(after, before);
    let backup = clone_with(&server, &token_of("backup"));
    assert_eq!(backup.ok(&["rev-parse", "HEAD"]).trim(), head);
    let reply = server.get_with(
        "/git/wiki.git/info/refs?service=git-receive-pack",
        &[("authorization", &bearer(&token_of("studio")))],
    );
    assert_eq!(reply.status, 200, "studio may still push");
}
