use serde_json::Value;

use crate::git::{self, Work};
use crate::server::Server;

pub fn all_pages(server: &Server) -> Vec<Value> {
    server.json("/v1/pages?deleted=true&limit=500")["pages"]
        .as_array()
        .unwrap()
        .clone()
}

pub fn live_page(server: &Server, path: &str) -> Value {
    server.json("/v1/pages?limit=500")["pages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|page| page["path"] == path)
        .unwrap_or_else(|| panic!("no page at {path}"))
        .clone()
}

pub fn events(server: &Server, page: &Value) -> Vec<Value> {
    let id = page["id"].as_str().unwrap();
    server.json(&format!("/v1/pages/{id}/versions?limit=500"))["events"]
        .as_array()
        .unwrap()
        .clone()
}

/// For every page, the versions the API counts are the commits in which `git log --follow`
/// sees its blob change.
pub fn assert_versions_match_git(server: &Server, work: &Work) {
    let pages = all_pages(server);
    assert_ne!(pages.len(), 0);
    for page in &pages {
        let events = events(server, page);
        let (last, created) = (events.first().unwrap(), events.last().unwrap());
        let path = page["path"].as_str().unwrap();
        let expected = work.text_changes(
            path,
            created["commit"].as_str().unwrap(),
            last["commit"].as_str().unwrap(),
        );
        assert_eq!(page["versions"], expected, "{path}");
        let versions = events
            .iter()
            .filter(|event| event["version"] == true)
            .count();
        assert_eq!(versions, expected, "{path}");
    }
}

fn kinds(events: &[Value]) -> Vec<(String, bool)> {
    events
        .iter()
        .map(|event| {
            let kind = event["kind"].as_str().unwrap();
            let kind = match event["from"].as_str() {
                Some(from) => format!("{kind} from {from}"),
                None => kind.to_owned(),
            };
            (kind, event["version"] == true)
        })
        .collect()
}

#[test]
fn every_page_has_as_many_versions_as_git_log_follow_finds() {
    let work = git::standard();
    let server = Server::of(&work);
    assert_eq!(all_pages(&server).len(), 6);
    assert_versions_match_git(&server, &work);
}

#[test]
fn a_rename_keeps_the_page_and_is_not_a_version() {
    let work = git::standard();
    let server = Server::of(&work);
    let alpha = live_page(&server, "greek/alpha.md");
    assert_eq!(
        kinds(&events(&server, &alpha)),
        [
            ("edited".to_owned(), true),
            ("renamed from alpha.md".to_owned(), false),
            ("edited".to_owned(), true),
            ("created".to_owned(), true),
        ]
    );
    assert_eq!(alpha["versions"], 3);
}

#[test]
fn a_rename_with_an_edit_is_a_version() {
    let work = git::standard();
    let server = Server::of(&work);
    let memories = live_page(&server, "minnen från gotland.md");
    let renamed = &events(&server, &memories)[0];
    assert_eq!(renamed["kind"], "renamed");
    assert_eq!(renamed["from"], "gotländska minnen.md");
    assert_eq!(renamed["version"], true);
    assert_eq!(
        (&renamed["added"], &renamed["removed"]),
        (&1.into(), &1.into())
    );
    assert_eq!(memories["title"], "Gotländska minnen");
}

#[test]
fn a_page_recreated_at_the_same_path_is_a_new_page() {
    let work = git::standard();
    let server = Server::of(&work);
    let deep: Vec<_> = all_pages(&server)
        .into_iter()
        .filter(|page| page["path"] == "notes/deep.md")
        .collect();
    assert_eq!(deep.len(), 2);
    assert_ne!(deep[0]["id"], deep[1]["id"]);
    let (old, new) = if deep[0]["deleted"].is_null() {
        (&deep[1], &deep[0])
    } else {
        (&deep[0], &deep[1])
    };
    assert_eq!(
        old["title"], "deep",
        "a page without a heading has its file name as title"
    );
    assert_eq!(new["title"], "Deep again");
    let deletion = &events(&server, old)[0];
    assert_eq!(deletion["kind"], "deleted");
    assert_eq!(deletion["version"], false);
    assert_eq!(deletion["removed"], 5);
    let deleted = server.json(&format!("/v1/pages/{}", old["id"].as_str().unwrap()));
    assert_eq!(deleted["markdown"], Value::Null);
}

#[test]
fn files_that_are_not_pages_are_ignored() {
    let work = git::standard();
    let server = Server::of(&work);
    let listing = server.json("/v1/pages?sort=title");
    let paths: Vec<_> = listing["pages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|page| page["path"].as_str().unwrap())
        .collect();
    assert_eq!(
        paths,
        [
            "AGENTS.md",
            "greek/alpha.md",
            "notes/deep.md",
            "minnen från gotland.md",
            "index.md"
        ]
    );
    assert_eq!(listing["total"], 5);
}

#[test]
fn the_overlap_trailer_and_the_author_are_recorded() {
    let work = git::standard();
    let server = Server::of(&work);
    let agents = events(&server, &live_page(&server, "AGENTS.md"));
    let start = work.ok(&["rev-list", "--max-parents=0", "main"]);
    assert_eq!(agents[0]["overlap"], start.trim());
    assert_eq!(agents[0]["overlap_author"], "Writer");
    assert_eq!(agents[1]["overlap"], Value::Null);
    assert_eq!(agents[1]["overlap_author"], Value::Null);
    assert_eq!(agents[0]["author"], "Writer");
    assert_eq!(
        agents[0]["time"],
        git::START + 8 * 600,
        "author time of the eighth commit"
    );
}

#[test]
fn the_author_of_the_edit_that_lost_is_read_from_main() {
    let work = Work::new();
    work.write("p.md", "# P\n\none\n");
    work.commit("Start");
    work.write("p.md", "# P\n\nfrom aeolus\n");
    work.ok(&["add", "-A"]);
    work.ok(&[
        "commit",
        "-q",
        "--author=aeolus <autosave@aeolus>",
        "-m",
        "Save p.md",
    ]);
    let lost = work.ok(&["rev-parse", "HEAD"]).trim().to_owned();
    work.ok(&["branch", "aside"]);
    work.write("p.md", "# P\n\nnot on main\n");
    work.ok(&["checkout", "-q", "aside"]);
    let aside = work.commit("A commit only on a branch");
    work.ok(&["checkout", "-q", "main"]);
    work.write("p.md", "# P\n\nfrom blackbox\n");
    work.commit(&format!("Save p.md\n\nOverlap: {lost}"));
    work.write("p.md", "# P\n\nagain\n");
    work.commit(&format!("Save p.md\n\nOverlap: {aside}"));
    work.ok(&["branch", "-D", "aside"]);
    let server = Server::of(&work);
    let changes = server.json("/v1/changes");
    let events = changes["events"].as_array().unwrap();
    assert_eq!(events[0]["overlap"], aside.as_str());
    assert_eq!(
        events[0]["overlap_author"],
        Value::Null,
        "a commit that is not on main has no author here"
    );
    assert_eq!(events[1]["overlap"], lost.as_str());
    assert_eq!(events[1]["overlap_author"], "aeolus");
    assert_eq!(events[1]["author"], "Writer");
    assert_eq!(events[2]["overlap_author"], Value::Null);
}

#[test]
fn changes_list_every_event_newest_first() {
    let work = git::standard();
    let server = Server::of(&work);
    let changes = server.json("/v1/changes?limit=3");
    assert_eq!(changes["total"], 13);
    let events = changes["events"].as_array().unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(events[0]["path"], "greek/alpha.md");
    assert_eq!(events[1]["path"], "AGENTS.md");
    let since = git::START + 6 * 600;
    let recent = server.json(&format!("/v1/changes?since={since}"));
    assert_eq!(recent["total"], 4);
}
