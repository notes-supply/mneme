use std::fs;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};

use crate::git::{Isolation, Work};
use crate::server::{MACHINES, Reply, Server};

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .cast_signed()
}

fn bearer(machine: &str) -> String {
    let (_, token) = MACHINES.iter().find(|(name, _)| *name == machine).unwrap();
    format!("Bearer {token}")
}

/// A Git request that carries `value` as its report.
fn report_raw(server: &Server, authorization: Option<&str>, value: &str) -> Reply {
    let mut headers = vec![("mneme-report", value)];
    headers.extend(authorization.map(|value| ("authorization", value)));
    server.get_with("/git/wiki.git/info/refs?service=git-upload-pack", &headers)
}

/// A Git request that carries `json` as its report, encoded as the service encodes it.
fn send(server: &Server, authorization: Option<&str>, json: &str) -> Reply {
    report_raw(server, authorization, &STANDARD.encode(json))
}

fn machine(server: &Server, name: &str) -> Value {
    let listing = server.json("/v1/machines");
    listing["machines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|machine| machine["name"] == name)
        .unwrap()
        .clone()
}

fn server_head(server: &Server) -> String {
    server.json("/v1/status")["head"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// A clone of `server` that sends `machine`'s token.
fn clone_as(server: &Server, machine: &str) -> Work {
    let header = format!("Authorization: {}", bearer(machine));
    Work::clone_from(&server.git_url(), &header)
}

#[test]
fn every_machine_is_listed_with_its_last_git_request_and_report() {
    let server = Server::empty(&Isolation::new());
    let listing = server.json("/v1/machines");
    assert_eq!(listing["total"], 3);
    let names: Vec<_> = listing["machines"]
        .as_array()
        .unwrap()
        .iter()
        .map(|machine| machine["name"].clone())
        .collect();
    assert_eq!(names, ["aeolus", "blackbox", "cassia"]);
    let aeolus = machine(&server, "aeolus");
    for key in [
        "seen",
        "reported",
        "state",
        "detail",
        "head",
        "skipped",
        "skipped_more",
    ] {
        assert_eq!(aeolus[key], Value::Null, "{key}");
    }
    assert_eq!(aeolus["current"], false);

    let before = now();
    let work = clone_as(&server, "blackbox");
    work.write("index.md", "# Home\n");
    work.commit("Start");
    let (ok, said) = work.push(&["origin", "main"]);
    assert!(ok, "{said}");
    let seen = machine(&server, "blackbox")["seen"].as_i64().unwrap();
    assert!((before..=now()).contains(&seen), "{seen}");

    let head = server_head(&server);
    let body = json!({
        "state": "idle",
        "detail": "",
        "head": head,
        "skipped": [{ "path": "link.md", "reason": "a symlink" }],
        "skipped_more": 2,
    })
    .to_string();
    let reply = send(&server, Some(&bearer("blackbox")), &body);
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert!(
        reply.body.contains("git-upload-pack"),
        "the Git request is answered"
    );
    let blackbox = machine(&server, "blackbox");
    assert_eq!(blackbox["state"], "idle");
    assert_eq!(blackbox["detail"], "");
    assert_eq!(blackbox["head"], head.as_str());
    assert_eq!(
        blackbox["skipped"],
        json!([{ "path": "link.md", "reason": "a symlink" }])
    );
    assert_eq!(blackbox["skipped_more"], 2);
    assert_eq!(blackbox["current"], true);
    assert!((before..=now()).contains(&blackbox["reported"].as_i64().unwrap()));

    let other = clone_as(&server, "aeolus");
    other.write("index.md", "# Home\n\nFrom aeolus.\n");
    other.commit("Edit");
    let (ok, said) = other.push(&["origin", "main"]);
    assert!(ok, "{said}");
    assert_eq!(
        machine(&server, "blackbox")["current"],
        false,
        "the server moved past the head blackbox reported"
    );

    let example = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/api/report.json"),
    )
    .unwrap();
    let reply = send(&server, Some(&bearer("cassia")), &example);
    assert_eq!(reply.status, 200, "{}", reply.body);
    let cassia = machine(&server, "cassia");
    assert_eq!(cassia["state"], "blocked");
    assert_eq!(cassia["detail"], "a merge is in progress");
    assert_eq!(
        cassia["seen"],
        Value::Null,
        "a request that carries a report is not a sync"
    );
    assert_eq!(
        server.post("/git/status", &[], example.as_bytes()).status,
        404,
        "reports have no route of their own"
    );
}

#[test]
fn a_report_needs_a_token_and_a_well_formed_header() {
    let server = Server::empty(&Isolation::new());
    let report = |state: &str, detail: &str, head: Value, skipped: Value| {
        json!({
            "state": state,
            "detail": detail,
            "head": head,
            "skipped": skipped,
            "skipped_more": 0,
        })
        .to_string()
    };
    let good = report("idle", "", Value::Null, json!([]));
    let unauthorized = send(&server, None, &good);
    assert_eq!(unauthorized.status, 401);
    assert_eq!(unauthorized.body, "");
    assert_eq!(send(&server, Some("Bearer not-a-token"), &good).status, 401);

    let blackbox = bearer("blackbox");
    for garbled in [
        report_raw(&server, Some(&blackbox), "not base64!"),
        send(&server, Some(&blackbox), "not json"),
    ] {
        assert_eq!(garbled.status, 400, "{}", garbled.body);
        assert!(garbled.json()["error"].is_string());
    }
    let link = |reason: &str| json!([{ "path": "link.md", "reason": reason }]);
    let many: Vec<Value> = (0..21)
        .map(|n| json!({ "path": format!("link-{n}.md"), "reason": "a symlink" }))
        .collect();
    for body in [
        json!({ "detail": "", "head": null, "skipped": [], "skipped_more": 0 }).to_string(),
        json!({ "state": "idle", "detail": "", "head": null }).to_string(),
        good.replace("\"skipped_more\":0", "\"skipped_more\":0,\"more\":1"),
        good.replace("\"skipped_more\":0", "\"skipped_more\":-1"),
        report("", "", Value::Null, json!([])),
        report("idle", "", json!("HEAD"), json!([])),
        report("idle", "", json!("A".repeat(40)), json!([])),
        report("idle", &"x".repeat(1025), Value::Null, json!([])),
        report("idle", "", Value::Null, Value::Array(many)),
        report("idle", "", Value::Null, link("")),
        report("idle", "", Value::Null, link(&"x".repeat(65))),
        report(
            "idle",
            "",
            Value::Null,
            json!([{ "path": "", "reason": "a symlink" }]),
        ),
        report(
            "idle",
            "",
            Value::Null,
            json!([{ "path": "x", "reason": "r", "why": 1 }]),
        ),
    ] {
        let reply = send(&server, Some(&blackbox), &body);
        assert_eq!(reply.status, 422, "{body}");
        assert!(reply.json()["error"].is_string(), "{body}");
    }
    let huge = report("idle", &"x".repeat(3 << 10), Value::Null, json!([]));
    assert_eq!(send(&server, Some(&blackbox), &huge).status, 413);
    let blackbox_now = machine(&server, "blackbox");
    assert_eq!(blackbox_now["state"], Value::Null, "no bad report was kept");
    assert_eq!(
        blackbox_now["seen"],
        Value::Null,
        "no refused request was a sync"
    );
    assert_eq!(send(&server, Some(&blackbox), &good).status, 200);
    let stored = machine(&server, "blackbox");
    assert_eq!(stored["state"], "idle");
    assert_eq!(stored["head"], Value::Null);
}

fn state_file(server: &Server) -> PathBuf {
    server.repo().join("mneme").join("machines.json")
}

/// The log lines of the server's loading of the machines' state.
fn loads(log: &[String]) -> Vec<Value> {
    log.iter()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line["event"] == "machines")
        .collect()
}

#[test]
fn machine_state_survives_a_restart_and_stays_out_of_git() {
    let git = Isolation::new();
    let server = Server::empty(&git);
    let first = loads(&server.log_with(r#""event":"machines""#));
    assert_eq!(first.len(), 1, "{first:?}");
    assert_eq!(first[0]["loaded"], 0);
    assert!(first[0]["error"].is_string(), "no file yet: {first:?}");

    let work = clone_as(&server, "blackbox");
    work.write("index.md", "# Home\n");
    work.commit("Start");
    let (ok, said) = work.push(&["origin", "main"]);
    assert!(ok, "{said}");
    let head = server_head(&server);
    let body = json!({
        "state": "blocked",
        "detail": "a merge is in progress",
        "head": head,
        "skipped": [{ "path": "link.md", "reason": "a symlink" }],
        "skipped_more": 1,
    })
    .to_string();
    assert_eq!(send(&server, Some(&bearer("blackbox")), &body).status, 200);
    let before = server.json("/v1/machines");
    assert_eq!(machine(&server, "blackbox")["state"], "blocked");

    let file = state_file(&server);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !fs::read_to_string(&file).is_ok_and(|text| text.contains("a merge is in progress")) {
        assert!(
            Instant::now() < deadline,
            "the state is written within a few seconds without a stop"
        );
        thread::sleep(Duration::from_millis(100));
    }

    let (status, log, server) = server.restart(&git);
    assert!(status.success(), "{status}");
    assert!(
        log.last().unwrap().contains(r#""event":"stopped""#),
        "{log:?}"
    );
    assert_eq!(
        server.json("/v1/machines"),
        before,
        "the same state after a restart"
    );
    let again = loads(&server.log());
    assert_eq!(again.len(), 1, "{again:?}");
    assert_eq!(again[0]["loaded"], 3);
    assert_eq!(again[0]["error"], Value::Null);

    let blob = git.ok(server.repo(), &["hash-object", file.to_str().unwrap()]);
    let blob = blob.trim();
    let refs = git.ok(server.repo(), &["for-each-ref", "--format=%(refname)"]);
    assert_eq!(refs, "refs/heads/main\n", "the state is no ref");
    let fresh = clone_as(&server, "aeolus");
    for repo in [server.repo(), fresh.path()] {
        let found = git.git(repo, &["cat-file", "-e", blob]);
        assert!(!found.status.success(), "the state is no object");
    }
    assert_eq!(fresh.ok(&["ls-files"]), "index.md\n");
    assert!(!fresh.path().join(".git").join("mneme").exists());
}

#[test]
fn an_unreadable_state_file_means_no_machine_seen() {
    let git = Isolation::new();
    let server = Server::empty(&git);
    let good = json!({
        "state": "idle", "detail": "", "head": null, "skipped": [], "skipped_more": 0,
    })
    .to_string();
    assert_eq!(send(&server, Some(&bearer("blackbox")), &good).status, 200);
    let (_, _, server) = server.restart(&git);
    assert_eq!(machine(&server, "blackbox")["state"], "idle");

    fs::write(state_file(&server), "{ not json").unwrap();
    let (status, _, server) = server.restart(&git);
    assert!(status.success(), "{status}");
    let blackbox = machine(&server, "blackbox");
    assert_eq!(blackbox["state"], Value::Null);
    assert_eq!(blackbox["reported"], Value::Null);
    let lines = loads(&server.log());
    assert_eq!(lines.len(), 1, "one line says why: {lines:?}");
    assert_eq!(lines[0]["loaded"], 0);
    assert!(lines[0]["error"].is_string(), "{lines:?}");
}

fn behind_since(server: &Server, name: &str) -> Value {
    machine(server, name)["behind_since"].clone()
}

fn report_head(server: &Server, name: &str, head: &str) {
    let body = json!({
        "state": "idle", "detail": "", "head": head, "skipped": [], "skipped_more": 0,
    })
    .to_string();
    let reply = send(server, Some(&bearer(name)), &body);
    assert_eq!(reply.status, 200, "{}", reply.body);
}

#[test]
fn a_machine_is_behind_from_when_its_head_first_differed() {
    let git = Isolation::new();
    let server = Server::empty(&git);
    let work = clone_as(&server, "blackbox");
    work.write("index.md", "# Home\n");
    work.commit("Start");
    let (ok, said) = work.push(&["origin", "main"]);
    assert!(ok, "{said}");
    let first = server_head(&server);
    assert_eq!(
        behind_since(&server, "blackbox"),
        Value::Null,
        "never reported"
    );
    report_head(&server, "blackbox", &first);
    assert_eq!(
        behind_since(&server, "blackbox"),
        Value::Null,
        "the same head"
    );
    assert_eq!(machine(&server, "blackbox")["current"], true);

    let pushed = now();
    let other = clone_as(&server, "aeolus");
    other.write("index.md", "# Home\n\nFrom aeolus.\n");
    other.commit("Edit");
    let (ok, said) = other.push(&["origin", "main"]);
    assert!(ok, "{said}");
    let since = behind_since(&server, "blackbox").as_i64().unwrap();
    assert!(
        (pushed..=now()).contains(&since),
        "set by the push: {since}"
    );
    assert_eq!(machine(&server, "blackbox")["current"], false);
    assert_eq!(
        behind_since(&server, "aeolus"),
        Value::Null,
        "aeolus never reported"
    );

    thread::sleep(Duration::from_millis(1100));
    report_head(&server, "blackbox", &first);
    assert_eq!(
        behind_since(&server, "blackbox"),
        since,
        "a report of the same old head keeps the first time"
    );
    let (_, _, server) = server.restart(&git);
    assert_eq!(
        behind_since(&server, "blackbox"),
        since,
        "kept across a restart"
    );

    let second = server_head(&server);
    report_head(&server, "blackbox", &second);
    assert_eq!(
        behind_since(&server, "blackbox"),
        Value::Null,
        "cleared by a match"
    );

    work.ok(&["remote", "set-url", "origin", &server.git_url()]);
    work.ok(&["pull", "-q", "--ff-only"]);
    work.write("local.md", "# Not pushed\n");
    let local = work.commit("Not pushed yet");
    let reported = now();
    report_head(&server, "blackbox", &local);
    let ahead = behind_since(&server, "blackbox").as_i64().unwrap();
    assert!(
        (reported..=now()).contains(&ahead),
        "a report of a head that is not the server's sets it: {ahead}"
    );
    let (ok, said) = work.push(&["origin", "main"]);
    assert!(ok, "{said}");
    assert_eq!(
        behind_since(&server, "blackbox"),
        Value::Null,
        "the push made the server's head the reported one"
    );
}
