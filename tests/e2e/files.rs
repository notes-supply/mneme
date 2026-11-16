//! Files that are not pages: the listing, one file's history, and the events of all of them.

use serde_json::Value;

use crate::git::{self, Work};
use crate::server::Server;

/// The bytes of a small file that is not text, made up from `seed`.
pub fn bytes(seed: u64, length: usize) -> Vec<u8> {
    let mut rng = fastrand::Rng::with_seed(seed);
    std::iter::repeat_with(|| rng.u8(..)).take(length).collect()
}

/// A wiki with a brand folder: a page, and files that are created, edited, moved with the same
/// bytes, moved with other bytes, and deleted.
pub fn brand() -> Work {
    let work = Work::new();
    work.write(
        "index.md",
        "# Home\n\nThe brand kit is in [brand](brand/).\n",
    );
    work.write(
        "brand/logo.svg",
        "<svg xmlns=\"http://www.w3.org/2000/svg\"><circle r=\"4\"/></svg>\n",
    );
    work.write_bytes("brand/kit.zip", &bytes(1, 3000));
    work.write_bytes("brand/Årets logga 2026.png", &bytes(2, 700));
    work.write("brand/colours.txt", "Ink #111111\nPaper #fafafa\n");
    work.write("brandy.txt", "Not in the brand folder.\n");
    work.write("notes/readme.txt", "Not a page.\n");
    work.commit("Start the wiki with a brand kit");

    work.write(
        "brand/logo.svg",
        "<svg xmlns=\"http://www.w3.org/2000/svg\"><circle r=\"5\"/></svg>\n",
    );
    work.commit("Round the logo");

    work.rename("brand/kit.zip", "brand/archive/Kit 2026.zip");
    work.commit("Move the kit");

    work.remove("notes/readme.txt");
    work.rename("brand/colours.txt", "brand/palette.txt");
    work.write("brand/palette.txt", "Ink #111111\nPaper #ffffff\n");
    work.commit("Delete the readme, and move the colours with a new paper");

    work.write(
        "index.md",
        "# Home\n\nThe brand kit is in [brand](brand/), with a mark.\n",
    );
    work.write_bytes("brand/mark.png", &bytes(3, 12_000));
    work.commit("Add a mark");
    work
}

fn paths(listing: &Value) -> Vec<&str> {
    listing["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| file["path"].as_str().unwrap())
        .collect()
}

/// Each event as its kind, the path it came from if it has one, and its size.
fn kinds(events: &Value) -> Vec<(String, u64)> {
    events
        .as_array()
        .unwrap()
        .iter()
        .map(|event| {
            let kind = event["kind"].as_str().unwrap();
            let kind = match event["from"].as_str() {
                Some(from) => format!("{kind} from {from}"),
                None => kind.to_owned(),
            };
            (kind, event["size"].as_u64().unwrap())
        })
        .collect()
}

#[test]
fn files_are_listed_with_their_size_type_and_newest_event() {
    let work = brand();
    let server = Server::of(&work);
    let listing = server.json("/v1/files");
    assert_eq!(listing["total"], 6);
    assert_eq!(
        paths(&listing),
        [
            "brand/archive/Kit 2026.zip",
            "brand/logo.svg",
            "brand/mark.png",
            "brand/palette.txt",
            "brand/Årets logga 2026.png",
            "brandy.txt",
        ],
        "live files that are not pages, by path"
    );
    let logo = &listing["files"][1];
    let commits: Vec<String> = work
        .ok(&["log", "--format=%H", "--", "brand/logo.svg"])
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(logo["size"], 62);
    assert_eq!(logo["type"], "image/svg+xml");
    assert_eq!(logo["created"], git::START + 600);
    assert_eq!(logo["updated"], git::START + 2 * 600);
    assert_eq!(logo["versions"], 2);
    assert_eq!(logo["author"], "Writer");
    assert_eq!(logo["commit"], commits[0].as_str());
    assert_eq!(logo["deleted"], Value::Null);
    let kit = &listing["files"][0];
    assert_eq!(kit["size"], 3000);
    assert_eq!(kit["type"], "application/octet-stream");
    assert_eq!(kit["created"], git::START + 600);
    assert_eq!(
        kit["updated"],
        git::START + 3 * 600,
        "the move is its newest event"
    );
    assert_eq!(
        kit["versions"], 1,
        "a move with the same bytes is not a version"
    );
    assert_eq!(listing["files"][3]["type"], "text/plain; charset=utf-8");
    assert_eq!(listing["files"][4]["type"], "image/png");
}

#[test]
fn files_are_found_under_a_folder_by_words_and_in_an_order() {
    let work = brand();
    let server = Server::of(&work);
    let list = |query: &str| -> (u64, Vec<String>) {
        let listing = server.json(&format!("/v1/files?{query}"));
        let paths = paths(&listing).into_iter().map(str::to_owned).collect();
        (listing["total"].as_u64().unwrap(), paths)
    };
    let (total, under) = list("under=brand");
    assert_eq!(total, 5, "brandy.txt is not inside brand");
    assert!(
        under.iter().all(|path| path.starts_with("brand/")),
        "{under:?}"
    );
    assert_eq!(list("under=brand/").0, 5);
    assert_eq!(
        list("under=brand/archive").1,
        ["brand/archive/Kit 2026.zip"]
    );
    assert_eq!(list("under=bran").0, 0);
    assert_eq!(list("under=").0, 6);
    assert_eq!(list("q=KIT%20zip").1, ["brand/archive/Kit 2026.zip"]);
    assert_eq!(list("q=%C3%A5rets").1, ["brand/Årets logga 2026.png"]);
    assert_eq!(list("q=brand%20png&under=brand").0, 2);
    assert_eq!(list("q=logo%20nothing").0, 0);
    assert_eq!(
        list("sort=size").1[..2],
        ["brand/mark.png", "brand/archive/Kit 2026.zip"],
        "the largest first"
    );
    assert_eq!(list("sort=size&order=asc&limit=1").1, ["brandy.txt"]);
    assert_eq!(
        list("sort=updated").1[..2],
        ["brand/mark.png", "brand/palette.txt"],
        "the newest event first"
    );
    assert_eq!(list("order=desc&limit=1").1, ["brandy.txt"]);
    let (total, page) = list("limit=2&offset=1");
    assert_eq!(total, 6);
    assert_eq!(page, ["brand/logo.svg", "brand/mark.png"]);
    for bad in ["sort=title", "order=up", "limit=x"] {
        let reply = server.get(&format!("/v1/files?{bad}"));
        assert_eq!(reply.status, 400, "{bad}");
        assert!(reply.json()["error"].is_string(), "{bad}");
    }
}

#[test]
fn a_file_keeps_its_history_across_a_move_with_the_same_bytes() {
    let work = brand();
    let server = Server::of(&work);
    let kit = server.json("/v1/files/brand/archive/Kit%202026.zip");
    assert_eq!(kit["file"]["path"], "brand/archive/Kit 2026.zip");
    assert_eq!(kit["file"]["versions"], 1);
    assert_eq!(kit["total"], 2);
    assert_eq!(
        kinds(&kit["events"]),
        [
            ("renamed from brand/kit.zip".to_owned(), 3000),
            ("created".to_owned(), 3000)
        ]
    );
    let moved = &kit["events"][0];
    assert_eq!(moved["path"], "brand/archive/Kit 2026.zip");
    assert_eq!(moved["time"], git::START + 3 * 600);
    assert_eq!(moved["author"], "Writer");
    assert_eq!(moved["commit"], kit["file"]["commit"]);
    assert_eq!(kit["events"][1]["path"], "brand/kit.zip");
    let old = server.get("/v1/files/brand/kit.zip");
    assert_eq!(
        old.status, 404,
        "the file moved away and was not deleted there"
    );
    assert_eq!(
        old.json()["error"],
        "no file is at this path, and none was deleted there"
    );
    let logo = server.json("/v1/files/brand/logo.svg?limit=1");
    assert_eq!(logo["total"], 2);
    assert_eq!(kinds(&logo["events"]), [("edited".to_owned(), 62)]);
    let first = server.json("/v1/files/brand/logo.svg?limit=1&offset=1");
    assert_eq!(kinds(&first["events"]), [("created".to_owned(), 62)]);
}

#[test]
fn a_move_with_other_bytes_is_a_deletion_and_a_creation() {
    let work = brand();
    let server = Server::of(&work);
    let palette = server.json("/v1/files/brand/palette.txt");
    assert_eq!(kinds(&palette["events"]), [("created".to_owned(), 26)]);
    assert_eq!(palette["file"]["created"], git::START + 4 * 600);
    let colours = server.json("/v1/files/brand/colours.txt");
    assert_eq!(
        kinds(&colours["events"]),
        [("deleted".to_owned(), 0), ("created".to_owned(), 26)]
    );
    assert_eq!(colours["file"]["deleted"], git::START + 4 * 600);
}

#[test]
fn a_deleted_file_is_found_at_its_path_until_another_is_live_there() {
    let work = brand();
    work.write("notes/readme.txt", "A second file at the path.\n");
    work.commit("Write the readme again");
    work.remove("notes/readme.txt");
    work.commit("Delete it again");
    let server = Server::of(&work);
    let gone = server.json("/v1/files/notes/readme.txt");
    assert_eq!(gone["file"]["deleted"], git::START + 7 * 600);
    assert_eq!(gone["file"]["size"], 27, "the size it last had");
    assert_eq!(gone["file"]["created"], git::START + 6 * 600);
    assert_eq!(
        kinds(&gone["events"]),
        [("deleted".to_owned(), 0), ("created".to_owned(), 27)],
        "the file most recently deleted at the path"
    );
    assert!(
        !paths(&server.json("/v1/files")).contains(&"notes/readme.txt"),
        "a deleted file is not listed"
    );
    assert_eq!(server.get("/v1/files/notes/never.txt").status, 404);
    assert_eq!(
        server.get("/v1/files/index.md").status,
        404,
        "a page is not a file"
    );

    work.write("notes/readme.txt", "A third.\n");
    work.commit("And once more");
    let server = Server::of(&work);
    let live = server.json("/v1/files/notes/readme.txt");
    assert_eq!(live["file"]["deleted"], Value::Null);
    assert_eq!(kinds(&live["events"]), [("created".to_owned(), 9)]);
}

#[test]
fn the_events_of_all_files_are_listed_newest_first_and_pages_are_not_among_them() {
    let work = brand();
    let server = Server::of(&work);
    let changes = server.json("/v1/changes/files");
    assert_eq!(changes["total"], 12);
    let events = changes["events"].as_array().unwrap();
    assert_eq!(events[0]["path"], "brand/mark.png");
    assert_eq!(events[0]["kind"], "created");
    assert!(
        events
            .iter()
            .all(|event| !mneme::repo::is_page(event["path"].as_str().unwrap().as_bytes())),
        "no page is a file"
    );
    let since = git::START + 3 * 600;
    let recent = server.json(&format!("/v1/changes/files?since={since}"));
    assert_eq!(recent["total"], 5);
    let older = server.json(&format!("/v1/changes/files?before={since}"));
    assert_eq!(
        older["total"], 7,
        "before leaves out the events at its time"
    );
    let window = server.json(&format!(
        "/v1/changes/files?since={since}&before={}&limit=1",
        since + 600
    ));
    assert_eq!(window["total"], 1);
    assert_eq!(
        kinds(&window["events"]),
        [("renamed from brand/kit.zip".to_owned(), 3000)]
    );
    let page = server.json("/v1/changes/files?limit=2&offset=10");
    assert_eq!(page["events"].as_array().unwrap().len(), 2);
    assert_eq!(
        server.json("/v1/changes")["total"],
        2,
        "the changes of pages are still pages only"
    );
    assert_eq!(server.get("/v1/changes/files?since=x").status, 400);
}

#[test]
fn files_with_the_same_bytes_move_in_path_order_and_a_copy_is_a_creation() {
    let work = Work::new();
    work.write("a/one.txt", "the same bytes\n");
    work.write("a/two.txt", "the same bytes\n");
    work.write("keep.txt", "the same bytes\n");
    work.commit("Three files with the same bytes");
    work.rename("a/one.txt", "b/first.txt");
    work.rename("a/two.txt", "b/second.txt");
    work.write("b/third.txt", "the same bytes\n");
    work.commit("Move two and copy one");
    let server = Server::of(&work);
    let from = |path: &str| server.json(&format!("/v1/files/{path}"))["events"][0]["from"].clone();
    assert_eq!(from("b/first.txt"), "a/one.txt");
    assert_eq!(from("b/second.txt"), "a/two.txt");
    assert_eq!(from("b/third.txt"), Value::Null);
    assert_eq!(server.json("/v1/files")["total"], 4);
}

#[test]
fn a_symlink_and_a_submodule_are_never_files() {
    let work = Work::new();
    work.write("real.txt", "A real file.\n");
    std::os::unix::fs::symlink("real.txt", work.file("link.txt")).unwrap();
    let nested = work.file("vendor/lib");
    std::fs::create_dir_all(&nested).unwrap();
    work.git.ok(&nested, &["init", "-q", "-b", "main"]);
    std::fs::write(nested.join("lib.txt"), "Inside a repository of its own.\n").unwrap();
    work.git.ok(&nested, &["add", "-A"]);
    work.git.ok(&nested, &["commit", "-q", "-m", "Start"]);
    work.commit("A file, a symlink and a submodule");
    assert!(
        work.ok(&["ls-tree", "HEAD", "vendor/lib"])
            .starts_with("160000 commit"),
        "the nested repository is stored as a submodule"
    );
    std::fs::remove_file(work.file("link.txt")).unwrap();
    work.write("link.txt", "Now a real file.\n");
    work.write("real.txt", "A real file, edited.\n");
    work.commit("Turn the symlink into a file");
    std::fs::remove_file(work.file("real.txt")).unwrap();
    std::os::unix::fs::symlink("link.txt", work.file("real.txt")).unwrap();
    work.commit("Turn the file into a symlink");
    let server = Server::of(&work);
    assert_eq!(paths(&server.json("/v1/files")), ["link.txt"]);
    let link = server.json("/v1/files/link.txt");
    assert_eq!(kinds(&link["events"]), [("created".to_owned(), 17)]);
    let real = server.json("/v1/files/real.txt");
    assert_eq!(
        kinds(&real["events"]),
        [
            ("deleted".to_owned(), 0),
            ("edited".to_owned(), 21),
            ("created".to_owned(), 13)
        ]
    );
    assert_eq!(server.get("/v1/files/vendor/lib").status, 404);
}

/// Two names that differ only in bytes that are not UTF-8 would be one path once those bytes
/// were replaced, and a change to one would land on the other. Such files are left out.
#[test]
fn files_whose_paths_are_not_utf8_are_left_out() {
    use std::ffi::OsStr;
    use std::os::unix::ffi::OsStrExt;

    let work = Work::new();
    let name = |bytes: &[u8]| work.path().join(OsStr::from_bytes(bytes));
    std::fs::write(name(b"caf\xe9.png"), "one\n").unwrap();
    std::fs::write(name(b"caf\xe8.png"), "two\n").unwrap();
    work.write("kept.txt", "A file with a plain name.\n");
    work.write("index.md", "# Home\n");
    work.commit("Two names that are not UTF-8");
    std::fs::write(name(b"caf\xe9.png"), "one, edited\n").unwrap();
    std::fs::rename(name(b"caf\xe8.png"), name(b"moved-\xff.png")).unwrap();
    work.commit("Edit one and move the other");
    std::fs::remove_file(name(b"caf\xe9.png")).unwrap();
    std::fs::remove_file(name(b"moved-\xff.png")).unwrap();
    work.commit("Delete both");
    std::fs::write(name(b"again-\xe9.bin"), "three\n").unwrap();
    std::fs::rename(work.file("kept.txt"), name(b"kept-\xe9.txt")).unwrap();
    work.commit("Add another, and move a plain name to one that is not UTF-8");
    let server = Server::of(&work);
    let listing = server.json("/v1/files");
    assert_eq!(listing["total"], 0, "{listing}");
    let kept = server.json("/v1/files/kept.txt");
    assert_eq!(
        kinds(&kept["events"]),
        [("deleted".to_owned(), 0), ("created".to_owned(), 26)],
        "a move to a name that is not UTF-8 leaves the files"
    );
    let changes = server.json("/v1/changes/files");
    assert_eq!(changes["total"], 2, "only kept.txt's events: {changes}");
    assert_eq!(
        server.json("/v1/pages")["total"],
        1,
        "pages are as they were"
    );
}
