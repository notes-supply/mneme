use std::time::{SystemTime, UNIX_EPOCH};

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

fn report(server: &Server, authorization: Option<&str>, body: &str) -> Reply {
    let mut headers = vec![("content-type", "application/json")];
    headers.extend(authorization.map(|value| ("authorization", value)));
    server.post("/git/status", &headers, body.as_bytes())
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
    for key in ["seen", "reported", "state", "detail", "head"] {
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
    let body = json!({ "state": "idle", "detail": "", "head": head }).to_string();
    let reply = report(&server, Some(&bearer("blackbox")), &body);
    assert_eq!(reply.status, 204, "{}", reply.body);
    let blackbox = machine(&server, "blackbox");
    assert_eq!(blackbox["state"], "idle");
    assert_eq!(blackbox["detail"], "");
    assert_eq!(blackbox["head"], head.as_str());
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
    let reply = report(&server, Some(&bearer("cassia")), &example);
    assert_eq!(reply.status, 204, "{}", reply.body);
    let cassia = machine(&server, "cassia");
    assert_eq!(cassia["state"], "blocked");
    assert_eq!(cassia["detail"], "a merge is in progress");
    assert_eq!(cassia["seen"], Value::Null, "a report is not a Git request");
}

#[test]
fn a_report_needs_a_token_and_a_well_formed_body() {
    let server = Server::empty(&Isolation::new());
    let good = json!({ "state": "idle", "detail": "", "head": null }).to_string();
    let unauthorized = report(&server, None, &good);
    assert_eq!(unauthorized.status, 401);
    assert_eq!(unauthorized.body, "");
    assert_eq!(
        report(&server, Some("Bearer not-a-token"), &good).status,
        401
    );

    let blackbox = bearer("blackbox");
    let not_json = report(&server, Some(&blackbox), "not json");
    assert_eq!(not_json.status, 400);
    assert!(not_json.json()["error"].is_string());
    for body in [
        json!({ "detail": "", "head": null }).to_string(),
        json!({ "state": "idle", "detail": "", "head": null, "more": 1 }).to_string(),
        json!({ "state": "", "detail": "", "head": null }).to_string(),
        json!({ "state": "idle", "detail": "", "head": "HEAD" }).to_string(),
        json!({ "state": "idle", "detail": "", "head": "A".repeat(40) }).to_string(),
        json!({ "state": "idle", "detail": "x".repeat(1025), "head": null }).to_string(),
    ] {
        let reply = report(&server, Some(&blackbox), &body);
        assert_eq!(reply.status, 422, "{body}");
        assert!(reply.json()["error"].is_string(), "{body}");
    }
    let huge = json!({ "state": "idle", "detail": "x".repeat(8 << 10), "head": null });
    assert_eq!(
        report(&server, Some(&blackbox), &huge.to_string()).status,
        413
    );
    assert_eq!(machine(&server, "blackbox")["state"], Value::Null);
    assert_eq!(report(&server, Some(&blackbox), &good).status, 204);
    let stored = machine(&server, "blackbox");
    assert_eq!(stored["state"], "idle");
    assert_eq!(stored["head"], Value::Null);
}
