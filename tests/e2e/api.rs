use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::git::{self, Work};
use crate::history::{assert_versions_match_git, events, live_page};
use crate::server::Server;

fn id(page: &Value) -> &str {
    page["id"].as_str().unwrap()
}

fn texts(lines: &Value, kind: &str) -> Vec<String> {
    lines
        .as_array()
        .unwrap()
        .iter()
        .filter(|line| line["kind"] == kind)
        .map(|line| {
            line["value"]
                .as_array()
                .unwrap()
                .iter()
                .map(|span| span["text"].as_str().unwrap())
                .collect()
        })
        .collect()
}

#[test]
fn a_page_has_its_markdown_and_four_windows() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
        .cast_signed();
    let day = 86_400;
    let work = Work::new();
    work.write("log.md", "# Log\n\none\n");
    work.commit_at("Start", now - 200 * day);
    work.write("log.md", "# Log\n\none two\n");
    work.commit_at("Edit", now - 60 * day);
    work.write("log.md", "# Log\n\none two three\n");
    work.commit_at("Edit", now - 2 * day);
    work.write("log.md", "# Log\n\none two three four\n");
    work.commit_at("Edit", now - 2 * day + 29 * 60);
    let server = Server::of(&work);
    let page = server.json(&format!("/v1/pages/{}", id(&live_page(&server, "log.md"))));
    assert_eq!(page["markdown"], "# Log\n\none two three four\n");
    assert_eq!(page["page"]["words"], 6);
    let windows: Vec<_> = page["windows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| {
            (
                w["days"].clone(),
                w["versions"].clone(),
                w["sessions"].clone(),
            )
        })
        .collect();
    assert_eq!(
        windows,
        [
            (7.into(), 2.into(), 1.into()),
            (30.into(), 2.into(), 1.into()),
            (90.into(), 3.into(), 2.into()),
            (Value::Null, 4.into(), 3.into()),
        ]
    );
    assert_eq!(page["windows"][3]["added"], 6);
    assert_versions_match_git(&server, &work);
}

#[test]
fn a_diff_defaults_to_the_last_change_of_the_current_text() {
    let work = git::standard();
    let server = Server::of(&work);
    let alpha = live_page(&server, "greek/alpha.md");
    let events = events(&server, &alpha);
    let diff = server.json(&format!("/v1/pages/{}/diff", id(&alpha)));
    assert_eq!(diff["to"], events[0]["commit"]);
    assert_eq!(
        diff["from"], events[1]["commit"],
        "the rename kept the text"
    );
    assert_eq!(texts(&diff["lines"], "added"), ["", "And a third."]);
    assert_eq!(texts(&diff["lines"], "removed"), Vec::<String>::new());
}

#[test]
fn a_diff_to_a_version_compares_with_the_version_before() {
    let work = git::standard();
    let server = Server::of(&work);
    let alpha = live_page(&server, "greek/alpha.md");
    let events = events(&server, &alpha);
    let second = events[2]["commit"].as_str().unwrap();
    let diff = server.json(&format!("/v1/pages/{}/diff?to={second}", id(&alpha)));
    assert_eq!(diff["from"], events[3]["commit"]);
    assert_eq!(
        texts(&diff["lines"], "added"),
        ["", "Now with a second line."]
    );
    let first = events[3]["commit"].as_str().unwrap();
    let explicit = server.json(&format!(
        "/v1/pages/{}/diff?from={first}&to={second}",
        id(&alpha)
    ));
    assert_eq!(explicit, diff);
}

#[test]
fn the_diff_of_a_deleted_page_removes_its_text() {
    let work = git::standard();
    let server = Server::of(&work);
    let old = crate::history::all_pages(&server)
        .into_iter()
        .find(|page| page["path"] == "notes/deep.md" && !page["deleted"].is_null())
        .unwrap();
    let diff = server.json(&format!("/v1/pages/{}/diff", id(&old)));
    assert_eq!(diff["to"], events(&server, &old)[0]["commit"]);
    assert_eq!(
        texts(&diff["lines"], "removed"),
        ["Deep note without a heading."]
    );
}

#[test]
fn the_text_at_a_commit_is_the_text_its_events_left() {
    let work = git::standard();
    let server = Server::of(&work);
    let alpha = live_page(&server, "greek/alpha.md");
    let events = events(&server, &alpha);
    let created = events[3]["commit"].as_str().unwrap();
    let at = server.json(&format!("/v1/pages/{}/versions/{created}", id(&alpha)));
    assert_eq!(
        at["markdown"],
        "# Alpha\n\nThe first page talks about boats and harbours.\n"
    );
    assert_eq!(at["commit"], created);
    let deep = live_page(&server, "notes/deep.md");
    let missing = server.get(&format!("/v1/pages/{}/versions/{created}", id(&deep)));
    assert_eq!(missing.status, 404);
    assert_eq!(
        missing.json()["error"],
        "the page has no text at this commit"
    );
}

#[test]
fn lookups_that_find_nothing_are_404_with_a_message() {
    let work = git::standard();
    let server = Server::of(&work);
    let alpha = live_page(&server, "greek/alpha.md");
    let cases = [
        (
            "/v1/pages/0000000000000000".to_owned(),
            "no page has this id",
        ),
        (
            format!("/v1/pages/{}/versions/{}", id(&alpha), "0".repeat(40)),
            "no commit on main has this id",
        ),
        (
            format!("/v1/pages/{}/diff?from=abc", id(&alpha)),
            "no commit on main has this id",
        ),
        ("/v1/nothing".to_owned(), "no such route"),
    ];
    for (path, message) in cases {
        let reply = server.get(&path);
        assert_eq!(reply.status, 404, "{path}");
        assert_eq!(reply.json()["error"], message, "{path}");
    }
}

#[test]
fn bad_parameters_are_400_with_a_message() {
    let work = git::standard();
    let server = Server::of(&work);
    for path in [
        "/v1/pages?sort=size",
        "/v1/pages?limit=-1",
        "/v1/search",
        "/v1/changes?since=x",
    ] {
        let reply = server.get(path);
        assert_eq!(reply.status, 400, "{path}");
        assert!(reply.json()["error"].is_string(), "{path}");
    }
}

#[test]
fn pages_sort_order_filter_and_page() {
    let work = git::standard();
    let server = Server::of(&work);
    let paths = |query: &str| -> Vec<String> {
        server.json(&format!("/v1/pages?{query}"))["pages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|page| page["path"].as_str().unwrap().to_owned())
            .collect()
    };
    assert_eq!(paths("sort=versions&limit=1"), ["greek/alpha.md"]);
    assert_eq!(
        paths("sort=title&order=desc&limit=2&offset=1"),
        ["minnen från gotland.md", "notes/deep.md"]
    );
    assert_eq!(
        paths("min_versions=2&sort=title"),
        [
            "AGENTS.md",
            "greek/alpha.md",
            "minnen från gotland.md",
            "index.md"
        ]
    );
    assert_eq!(paths("max_words=9&sort=title"), ["index.md"]);
    assert_eq!(
        paths("deleted=true&sort=created&order=asc&limit=1"),
        ["AGENTS.md"]
    );
}

fn search_wiki() -> Work {
    let work = Work::new();
    work.write(
        "harbour.md",
        "# Harbour\n\nBoats rest in the old harbour at night.\n\n\
         See [the lighthouse](beacon-target.md) for more.\n\n```\nlet mooring = 7;\n```\n",
    );
    work.write("storm.md", "# Storm\n\nA storm hit the harbour.\n");
    work.write("café.md", "# Café\n\nCoffee at the café by the quay.\n");
    work.write("lighthouse.md", "# Lighthouse\n\nThe keeper lives alone.\n");
    work.commit("Write about the coast");
    work
}

fn hits(server: &Server, q: &str) -> Vec<String> {
    server.json(&format!("/v1/search?q={q}"))["hits"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| hit["page"]["path"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn search_finds_phrases_prefixes_and_exclusions() {
    let work = search_wiki();
    let server = Server::of(&work);
    assert_eq!(hits(&server, "%22old%20harbour%22"), ["harbour.md"]);
    let mut prefixed = hits(&server, "harb*");
    prefixed.sort();
    assert_eq!(prefixed, ["harbour.md", "storm.md"]);
    assert_eq!(hits(&server, "harbour%20-storm"), ["harbour.md"]);
    assert_eq!(hits(&server, "cafe"), ["café.md"], "diacritics are removed");
    assert_versions_match_git(&server, &work);
}

#[test]
fn search_reads_code_and_link_text_but_not_link_targets() {
    let work = search_wiki();
    let server = Server::of(&work);
    assert_eq!(hits(&server, "mooring"), ["harbour.md"]);
    assert_eq!(
        hits(&server, "lighthouse"),
        ["lighthouse.md", "harbour.md"],
        "the title weighs more than the text"
    );
    assert_eq!(hits(&server, "beacon"), Vec::<String>::new());
}

#[test]
fn search_snippets_mark_the_matched_words() {
    let work = search_wiki();
    let server = Server::of(&work);
    let hit = &server.json("/v1/search?q=quay")["hits"][0];
    let marked: Vec<_> = hit["snippet"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|mark| mark["hit"] == true)
        .map(|mark| mark["text"].as_str().unwrap())
        .collect();
    assert_eq!(marked, ["quay"]);
    assert!(hit["score"].as_f64().unwrap() > 0.0);
    let requests: Vec<_> = server
        .log_with(r#""event":"request""#)
        .into_iter()
        .filter(|line| line.contains(r#""event":"request""#))
        .collect();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].contains(r#""path":"/v1/search""#),
        "{}",
        requests[0]
    );
    let log = server.log().join("\n");
    assert!(
        !log.contains("quay"),
        "no search words or page text in the log: {log}"
    );
}

#[test]
fn a_broken_search_is_400_and_not_500() {
    let work = search_wiki();
    let server = Server::of(&work);
    for q in ["%22open", "a*b", "-storm", "%22%22", "%20"] {
        let reply = server.get(&format!("/v1/search?q={q}"));
        assert_eq!(reply.status, 400, "{q}: {}", reply.body);
        assert!(reply.json()["error"].is_string());
    }
    assert!(
        hits(&server, "NEAR(a").is_empty(),
        "FTS5 syntax is searched as words"
    );
}

#[test]
fn every_response_has_a_strong_etag_and_a_known_one_gets_304() {
    let work = git::standard();
    let server = Server::of(&work);
    let first = server.get("/v1/pages");
    let tag = first.header("etag").unwrap().to_owned();
    assert!(
        tag.starts_with('"') && tag.ends_with('"'),
        "{tag} is strong"
    );
    let again = server.get_with("/v1/pages", &[("if-none-match", &tag)]);
    assert_eq!(again.status, 304);
    assert_eq!(again.body, "");
    assert_eq!(again.header("etag"), Some(tag.as_str()));
    let other = server.get_with("/v1/pages", &[("if-none-match", "\"other\"")]);
    assert_eq!(other.status, 200);
    assert_eq!(other.body, first.body);
    let missing = server.get("/v1/pages/0000000000000000");
    assert!(missing.header("etag").is_some());
}

/// Why `actual` does not have the shape of `example`: the same keys, each with a value of
/// the same JSON type. `null` stands for an optional value on either side, and each element of
/// an array has the shape of some element of the example's array.
fn misfit(actual: &Value, example: &Value, at: &str) -> Option<String> {
    match (actual, example) {
        (_, Value::Null)
        | (Value::Null, _)
        | (Value::Bool(_), Value::Bool(_))
        | (Value::Number(_), Value::Number(_))
        | (Value::String(_), Value::String(_)) => None,
        (Value::Object(actual), Value::Object(example)) => {
            let mut keys: Vec<_> = actual.keys().collect();
            let mut expected: Vec<_> = example.keys().collect();
            keys.sort();
            expected.sort();
            if keys != expected {
                return Some(format!("{at}: keys {keys:?}, expected {expected:?}"));
            }
            actual
                .iter()
                .find_map(|(key, value)| misfit(value, &example[key], &format!("{at}.{key}")))
        }
        (Value::Array(actual), Value::Array(example)) if example.is_empty() => {
            (!actual.is_empty()).then(|| format!("{at}: the example array is empty"))
        }
        (Value::Array(actual), Value::Array(example)) => {
            actual.iter().enumerate().find_map(|(i, value)| {
                let at = format!("{at}[{i}]");
                let misfits: Vec<_> = example
                    .iter()
                    .filter_map(|e| misfit(value, e, &at))
                    .collect();
                (misfits.len() == example.len()).then(|| misfits.join("; "))
            })
        }
        _ => Some(format!(
            "{at}: {actual} does not have the shape of {example}"
        )),
    }
}

fn examples() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/api")
}

fn example(name: &str) -> Value {
    let path = examples().join(format!("{name}.json"));
    serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap()
}

#[test]
fn every_route_answers_in_the_shape_of_its_example_file() {
    let work = git::standard();
    let server = Server::of(&work);
    let alpha = live_page(&server, "greek/alpha.md");
    let commit = events(&server, &alpha)[3]["commit"]
        .as_str()
        .unwrap()
        .to_owned();
    let alpha = id(&alpha);
    let routes = [
        ("status", "/v1/status".to_owned()),
        ("pages", "/v1/pages?deleted=true".to_owned()),
        ("page", format!("/v1/pages/{alpha}")),
        ("versions", format!("/v1/pages/{alpha}/versions")),
        ("version", format!("/v1/pages/{alpha}/versions/{commit}")),
        ("diff", format!("/v1/pages/{alpha}/diff")),
        ("changes", "/v1/changes".to_owned()),
        ("search", "/v1/search?q=harbours".to_owned()),
    ];
    for (name, path) in routes {
        let actual = server.json(&path);
        if std::env::var_os("MNEME_WRITE_EXAMPLES").is_some() {
            let pretty = serde_json::to_string_pretty(&actual).unwrap();
            fs::write(examples().join(format!("{name}.json")), pretty + "\n").unwrap();
        }
        let example = example(name);
        assert_eq!(misfit(&actual, &example, name), None);
        assert_eq!(misfit(&example, &actual, name), None);
    }
}
