//! The editing API: each change is one commit on `main`, refused when it breaks a rule of the
//! wiki or starts from a base that is no longer the page's, and built again on a push that lands
//! as it is made.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use tempfile::TempDir;

use crate::git::{self, Isolation, Work};
use crate::history::events;
use crate::server::{AUTHORIZATION, MACHINES, Options, READER, Reply, Server, TOKEN};

/// A server with a home page pushed by a writer, and that writer's clone.
fn wiki(options: &Options) -> (Server, Work) {
    let server = Server::empty_with(&Isolation::new(), options);
    let work = Work::clone_from(&server.git_url(), AUTHORIZATION);
    work.write("index.md", "# Home\n\nThe start.\n");
    work.write("notes/plan.md", "# Plan\n\nOne step.\n");
    work.commit("Start");
    let (ok, said) = work.push(&["origin", "main"]);
    assert!(ok, "{said}");
    (server, work)
}

fn author() -> Value {
    json!({ "name": "Ada Writer", "email": "ada@example.invalid" })
}

/// A JSON request with `token`, or with no `Authorization` when it is `None`.
fn send(server: &Server, method: &str, path: &str, token: Option<&str>, body: &Value) -> Reply {
    let authorization = token.map(|token| format!("Bearer {token}"));
    let mut headers = vec![("content-type", "application/json")];
    headers.extend(
        authorization
            .as_deref()
            .map(|value| ("authorization", value)),
    );
    server.request(method, path, &headers, body.to_string().as_bytes())
}

fn put(server: &Server, path: &str, markdown: &str, base: Option<&str>) -> Reply {
    let body = json!({ "markdown": markdown, "base": base, "author": author() });
    send(
        server,
        "PUT",
        &format!("/v1/paths/{path}"),
        Some(TOKEN),
        &body,
    )
}

fn head(server: &Server) -> String {
    server.json("/v1/status")["head"]
        .as_str()
        .unwrap()
        .to_owned()
}

/// What `git log -1` says of `commit` in the server's repository, in `format`.
fn show(server: &Server, commit: &str, format: &str) -> String {
    Isolation::new()
        .ok(
            server.repo(),
            &["log", "-1", &format!("--format={format}"), commit],
        )
        .trim_end()
        .to_owned()
}

/// The base a client edits `path` from: its page's newest event, as `/v1/pages/{id}` gives it.
fn base(server: &Server, path: &str) -> String {
    let id = server.json(&format!("/v1/paths/{path}"))["id"]
        .as_str()
        .unwrap()
        .to_owned();
    server.json(&format!("/v1/pages/{id}"))["commit"]
        .as_str()
        .unwrap()
        .to_owned()
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

/// Reports the server's head as `machine`'s, as its autosave service would.
fn report_current(server: &Server, (name, token): (&str, &str)) {
    let report = json!({
        "state": "idle", "detail": "", "head": head(server), "skipped": [], "skipped_more": 0,
    });
    let reply = server.get_with(
        "/git/wiki.git/info/refs?service=git-upload-pack",
        &[
            ("authorization", &format!("Bearer {token}")),
            ("mneme-report", &STANDARD.encode(report.to_string())),
        ],
    );
    assert_eq!(reply.status, 200, "{name}: {}", reply.body);
}

#[test]
fn a_page_is_created_edited_moved_and_deleted_and_the_next_read_sees_each_change() {
    let (server, work) = wiki(&Options::default());
    report_current(&server, MACHINES[2]);
    assert_eq!(machine(&server, "tablet")["current"], true);

    let created = put(&server, "notes/a.md", "# A\n\nFirst.\n", None);
    assert_eq!(created.status, 201, "{}", created.body);
    let created = created.json();
    let commit = created["commit"].as_str().unwrap().to_owned();
    assert_eq!(head(&server), commit, "a read that follows sees the commit");
    assert_eq!(created["page"]["path"], "notes/a.md");
    assert_eq!(created["page"]["title"], "A");
    let id = created["page"]["id"].as_str().unwrap().to_owned();
    let page = server.json(&format!("/v1/pages/{id}"));
    assert_eq!(page["markdown"], "# A\n\nFirst.\n");
    assert_eq!(page["commit"], commit.as_str());
    assert_eq!(
        show(&server, &commit, "%an <%ae>|%cn <%ce>|%s|%P"),
        format!(
            "Ada Writer <ada@example.invalid>|laptop <api@laptop>|Create notes/a.md|{}",
            work.ok(&["rev-parse", "HEAD"]).trim()
        )
    );
    let tablet_now = machine(&server, "tablet");
    assert_eq!(tablet_now["current"], false, "as after a push");
    assert!(tablet_now["behind_since"].is_i64());

    let body = json!({
        "markdown": "# A\n\nFirst, and second.\n",
        "base": commit,
        "author": author(),
        "message": "Say more",
    });
    let edited = send(&server, "PUT", "/v1/paths/notes/a.md", Some(TOKEN), &body);
    assert_eq!(edited.status, 200, "{}", edited.body);
    let edited = edited.json();
    let second = edited["commit"].as_str().unwrap().to_owned();
    assert_eq!(edited["page"]["id"], id.as_str());
    assert_eq!(edited["page"]["versions"], 2);
    assert_eq!(
        show(&server, &second, "%s|%P"),
        format!("Say more|{commit}")
    );

    let same = put(
        &server,
        "notes/a.md",
        "# A\n\nFirst, and second.\n",
        Some(&second),
    );
    assert_eq!(same.status, 200, "{}", same.body);
    assert_eq!(same.json()["commit"], second.as_str(), "no commit is made");
    assert_eq!(head(&server), second);

    let body = json!({
        "from": "notes/a.md", "to": "archive/a.md", "base": second, "author": author(),
    });
    let moved = send(&server, "POST", "/v1/moves", Some(TOKEN), &body);
    assert_eq!(moved.status, 200, "{}", moved.body);
    let moved = moved.json();
    let third = moved["commit"].as_str().unwrap().to_owned();
    assert_eq!(moved["page"]["id"], id.as_str(), "the page keeps its id");
    assert_eq!(moved["page"]["path"], "archive/a.md");
    assert_eq!(
        show(&server, &third, "%s"),
        "Move notes/a.md to archive/a.md"
    );
    let newest = &events(&server, &moved["page"])[0];
    assert_eq!(
        (&newest["kind"], &newest["from"]),
        (&json!("renamed"), &json!("notes/a.md"))
    );

    let body = json!({ "base": third, "author": author() });
    let deleted = send(
        &server,
        "DELETE",
        "/v1/paths/archive/a.md",
        Some(TOKEN),
        &body,
    );
    assert_eq!(deleted.status, 200, "{}", deleted.body);
    let fourth = deleted.json()["commit"].as_str().unwrap().to_owned();
    assert_eq!(head(&server), fourth);
    assert!(server.json(&format!("/v1/pages/{id}"))["page"]["deleted"].is_i64());

    work.ok(&["pull", "-q", "--ff-only"]);
    assert_eq!(work.ok(&["rev-parse", "HEAD"]).trim(), fourth);
    assert!(!work.file("archive/a.md").exists());
    assert_eq!(
        work.ok(&["log", "--format=%s", "-4"]),
        "Delete archive/a.md\nMove notes/a.md to archive/a.md\nSay more\nCreate notes/a.md\n"
    );
    let edits = server
        .log_with(&fourth)
        .iter()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line["event"] == "edit" && line["machine"] == "laptop")
        .count();
    assert_eq!(edits, 4, "a change that changed nothing is no edit");
    assert!(!server.log().join("\n").contains("First, and second"));
}

#[test]
fn a_page_path_or_text_that_breaks_a_rule_is_422_and_changes_nothing() {
    let (server, _work) = wiki(&Options::default());
    let before = head(&server);
    let large = "a".repeat((1 << 20) + 1);
    let cases: Vec<(&str, String, Value, &str)> = vec![
        (
            "PUT",
            "notes/a.txt".into(),
            json!(null),
            "does not end in .md",
        ),
        (
            "PUT",
            "notes/a.MD".into(),
            json!(null),
            "does not end in .md",
        ),
        ("PUT", "/a.md".into(), json!(null), "starts with /"),
        (
            "PUT",
            "notes//a.md".into(),
            json!(null),
            "an empty, . or .. part",
        ),
        (
            "PUT",
            "notes/./a.md".into(),
            json!(null),
            "an empty, . or .. part",
        ),
        (
            "PUT",
            "notes/../a.md".into(),
            json!(null),
            "an empty, . or .. part",
        ),
        ("PUT", "a%00.md".into(), json!(null), "holds a NUL"),
        (
            "PUT",
            "x/.GIT/a.md".into(),
            json!(null),
            "a path named .git at x/.GIT/a.md",
        ),
        (
            "PUT",
            "git~1/a.md".into(),
            json!(null),
            "a name Git will not store",
        ),
        (
            "PUT",
            "index.md/a.md".into(),
            json!(null),
            "in the way of index.md/a.md",
        ),
        ("PUT", "notes.md".into(), json!(large), "a page over 1 MiB"),
    ];
    for (method, path, markdown, said) in cases {
        let markdown = markdown.as_str().unwrap_or("# A\n").to_owned();
        let body = json!({ "markdown": markdown, "base": null, "author": author() });
        let reply = send(
            &server,
            method,
            &format!("/v1/paths/{path}"),
            Some(TOKEN),
            &body,
        );
        assert_eq!(reply.status, 422, "{path}: {}", reply.body);
        let error = reply.json()["error"].as_str().unwrap().to_owned();
        assert!(error.contains(said), "{path}: {error}");
    }
    assert_eq!(head(&server), before, "nothing was committed");
}

#[test]
fn a_request_with_a_bad_author_base_message_or_body_is_refused_and_changes_nothing() {
    let (server, _work) = wiki(&Options::default());
    let before = head(&server);
    let page =
        |author: Value, base: Value| json!({ "markdown": "# A\n", "base": base, "author": author });
    let bodies = [
        (
            page(
                json!({ "name": " ", "email": "a@example.invalid" }),
                json!(null),
            ),
            "name is empty",
        ),
        (
            page(json!({ "name": "A", "email": "a@x\nb" }), json!(null)),
            "email is more than one line",
        ),
        (
            page(json!({ "name": "A <b>", "email": "a@x" }), json!(null)),
            "name holds <, > or NUL",
        ),
        (page(author(), json!("abc")), "not a full commit id"),
        (
            json!({ "markdown": "# A\n", "author": author() }),
            "missing field `base`",
        ),
        (
            json!({ "markdown": "# A\n", "base": null, "author": author(), "extra": 1 }),
            "unknown field",
        ),
        (
            json!({ "markdown": "# A\n", "base": null, "author": author(), "message": "a\0b" }),
            "NUL",
        ),
    ];
    for (body, said) in bodies {
        let reply = send(&server, "PUT", "/v1/paths/notes/a.md", Some(TOKEN), &body);
        assert_eq!(reply.status, 422, "{body}: {}", reply.body);
        let error = reply.json()["error"].as_str().unwrap().to_owned();
        assert!(error.contains(said), "{body}: {error}");
    }
    let plan = base(&server, "notes/plan.md");
    for (to, said) in [
        ("notes/plan.md", "from and to are both"),
        ("plan.txt", "does not end in .md"),
    ] {
        let body = json!({ "from": "notes/plan.md", "to": to, "base": plan, "author": author() });
        let reply = send(&server, "POST", "/v1/moves", Some(TOKEN), &body);
        assert_eq!(reply.status, 422, "{to}: {}", reply.body);
        assert!(
            reply.json()["error"].as_str().unwrap().contains(said),
            "{to}"
        );
    }
    let garbled = server.request(
        "PUT",
        "/v1/paths/notes/a.md",
        &[
            ("authorization", &format!("Bearer {TOKEN}")),
            ("content-type", "application/json"),
        ],
        b"{ not json",
    );
    assert_eq!(garbled.status, 400);
    assert!(garbled.json()["error"].is_string());
    let untyped = server.request(
        "PUT",
        "/v1/paths/notes/a.md",
        &[("authorization", &format!("Bearer {TOKEN}"))],
        page(author(), json!(null)).to_string().as_bytes(),
    );
    assert_eq!(untyped.status, 415);
    assert_eq!(head(&server), before, "nothing was committed");
}

#[test]
fn a_change_from_a_base_that_is_no_longer_the_pages_is_409_and_changes_nothing() {
    let (server, work) = wiki(&Options::default());
    let plan = base(&server, "notes/plan.md");
    let home = base(&server, "index.md");

    let taken = put(&server, "index.md", "# Home again\n", None);
    assert_eq!(taken.status, 409, "{}", taken.body);
    assert_eq!(taken.json()["current"], home.as_str());

    work.write("notes/plan.md", "# Plan\n\nTwo steps.\n");
    let pushed = work.commit("Plan more");
    assert!(work.push(&["origin", "main"]).0);
    let stale = put(&server, "notes/plan.md", "# Plan\n\nMine.\n", Some(&plan));
    assert_eq!(stale.status, 409, "{}", stale.body);
    assert_eq!(stale.json()["current"], pushed.as_str());
    let body = json!({ "base": plan, "author": author() });
    let deleting = send(
        &server,
        "DELETE",
        "/v1/paths/notes/plan.md",
        Some(TOKEN),
        &body,
    );
    assert_eq!(deleting.status, 409, "{}", deleting.body);
    assert_eq!(deleting.json()["current"], pushed.as_str());
    let body =
        json!({ "from": "notes/plan.md", "to": "plan.md", "base": plan, "author": author() });
    let moving = send(&server, "POST", "/v1/moves", Some(TOKEN), &body);
    assert_eq!(moving.status, 409, "{}", moving.body);
    assert_eq!(moving.json()["current"], pushed.as_str());
    let body =
        json!({ "from": "notes/plan.md", "to": "index.md", "base": pushed, "author": author() });
    let onto = send(&server, "POST", "/v1/moves", Some(TOKEN), &body);
    assert_eq!(onto.status, 409, "{}", onto.body);
    assert!(
        onto.json()["error"]
            .as_str()
            .unwrap()
            .contains("already at index.md")
    );
    assert_eq!(head(&server), pushed, "nothing was committed");

    let elsewhere = put(
        &server,
        "index.md",
        "# Home\n\nThe start, edited.\n",
        Some(&home),
    );
    assert_eq!(
        elsewhere.status, 200,
        "another page's change is no conflict: {}",
        elsewhere.body
    );
    let edit = elsewhere.json()["commit"].as_str().unwrap().to_owned();
    assert_eq!(
        show(&server, &edit, "%P"),
        pushed,
        "built on the current main"
    );

    work.ok(&["mv", "notes/plan.md", "notes/steps.md"]);
    work.commit("Rename the plan");
    work.ok(&["pull", "-q", "--rebase"]);
    assert!(work.push(&["origin", "main"]).0);
    let after_rename = put(&server, "notes/plan.md", "# Plan\n", Some(&pushed));
    assert_eq!(after_rename.status, 409, "{}", after_rename.body);
    assert_eq!(
        after_rename.json()["current"],
        Value::Null,
        "no page is there now"
    );
}

#[test]
fn the_first_page_of_an_empty_wiki_is_a_commit_with_no_parent() {
    let server = Server::empty(&Isolation::new());
    let body = json!({ "base": "0".repeat(40), "author": author() });
    let deleting = send(&server, "DELETE", "/v1/paths/index.md", Some(TOKEN), &body);
    assert_eq!(deleting.status, 409, "{}", deleting.body);
    assert_eq!(deleting.json()["current"], Value::Null);
    let created = put(&server, "index.md", "# Home\n", None);
    assert_eq!(created.status, 201, "{}", created.body);
    let commit = created.json()["commit"].as_str().unwrap().to_owned();
    assert_eq!(head(&server), commit);
    assert_eq!(show(&server, &commit, "%P"), "");
    let clone = Work::clone_from(&server.git_url(), AUTHORIZATION);
    assert_eq!(
        std::fs::read_to_string(clone.file("index.md")).unwrap(),
        "# Home\n"
    );
}

#[test]
fn every_change_asks_for_a_token_that_may_write() {
    for private_api in [false, true] {
        let options = Options {
            reader: true,
            private_api,
            ..Options::default()
        };
        let (server, _work) = wiki(&options);
        let body = json!({ "markdown": "# A\n", "base": null, "author": author() });
        for token in [None, Some("not-a-token")] {
            let reply = send(&server, "PUT", "/v1/paths/a.md", token, &body);
            assert_eq!(reply.status, 401, "{private_api} {token:?}");
            assert_eq!(reply.body, "");
            assert_eq!(reply.header("www-authenticate"), Some("Bearer"));
        }
        let read_only = send(&server, "PUT", "/v1/paths/a.md", Some(READER.1), &body);
        assert_eq!(read_only.status, 403, "{private_api}");
        assert!(
            read_only.json()["error"]
                .as_str()
                .unwrap()
                .contains("read-only")
        );
        let body = json!({ "from": "index.md", "to": "a.md", "base": null, "author": author() });
        let moving = send(&server, "POST", "/v1/moves", Some(READER.1), &body);
        assert_eq!(moving.status, 403, "{private_api}");
        assert_eq!(
            put(&server, "a.md", "# A\n", None).status,
            201,
            "{private_api}"
        );
    }
}

/// A push lands right before the server moves `main` to an edit. The edit is checked again
/// against the pushed `main`: built on top of it when the push changed another page, and
/// refused when it changed this one.
#[test]
fn a_push_that_lands_as_an_edit_is_made_is_kept() {
    for same_page in [false, true] {
        let bin = TempDir::new().unwrap();
        git::racy_git_on_path(bin.path());
        let options = Options {
            path: Some(bin.path().to_owned()),
            ..Options::default()
        };
        let (server, _work) = wiki(&options);
        let plan = base(&server, "notes/plan.md");
        let other = Work::clone_from(&server.git_url(), AUTHORIZATION);
        let pushed_path = if same_page {
            "notes/plan.md"
        } else {
            "index.md"
        };
        other.write(pushed_path, "# Pushed\n\nAs the edit was made.\n");
        let pushed = other.commit("Push as the edit is made");
        let script = format!(
            "unset GIT_DIR GIT_INDEX_FILE GIT_LITERAL_PATHSPECS\ncd '{}' && git push -q origin main\n",
            other.path().display()
        );
        std::fs::write(server.repo().join("mneme-test-race"), script).unwrap();
        std::fs::write(server.repo().join("mneme-test-race.before"), "update-ref").unwrap();

        let reply = put(&server, "notes/plan.md", "# Plan\n\nEdited.\n", Some(&plan));
        assert!(
            server.repo().join("mneme-test-race.ran").exists(),
            "the push raced the edit"
        );
        if same_page {
            assert_eq!(reply.status, 409, "{}", reply.body);
            assert_eq!(reply.json()["current"], pushed.as_str());
            assert_eq!(head(&server), pushed);
            continue;
        }
        assert_eq!(reply.status, 200, "{}", reply.body);
        let edit = reply.json()["commit"].as_str().unwrap().to_owned();
        assert_eq!(head(&server), edit);
        assert_eq!(
            show(&server, &edit, "%P"),
            pushed,
            "the edit is on top of the push"
        );
        let attempts: Vec<Value> = server
            .log()
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|line| line["event"] == "edit")
            .map(|line| line["attempts"].clone())
            .collect();
        assert_eq!(attempts, [2]);
        let index = server.json(&format!(
            "/v1/pages/{}",
            server.json("/v1/paths/index.md")["id"].as_str().unwrap()
        ));
        assert_eq!(index["markdown"], "# Pushed\n\nAs the edit was made.\n");
    }
}
