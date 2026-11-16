//! `GET /v1/raw/{path}`: a file's bytes, with headers that keep them from running in a browser.

use std::fs;

use mneme::rules::MAX_WHOLE;
use tempfile::TempDir;

use crate::files::{brand, bytes};
use crate::git::Work;
use crate::server::{AUTHORIZATION, Options, Reply, Server};

const SANDBOXED: &str = "default-src 'none'; style-src 'unsafe-inline'; sandbox";
const KIT: &str = "/v1/raw/brand/archive/Kit%202026.zip";

/// The blob `git` has at `path` in `commit`, as the `ETag` names it.
fn tag(work: &Work, commit: &str, path: &str) -> String {
    let blob = work.ok(&["rev-parse", &format!("{commit}:{path}")]);
    format!("\"{}\"", blob.trim())
}

fn header<'a>(reply: &'a Reply, name: &str) -> &'a str {
    reply
        .header(name)
        .unwrap_or_else(|| panic!("no {name} in {:?}", reply.headers))
}

#[test]
fn a_file_is_sent_whole_with_its_length_tag_type_and_name() {
    let work = brand();
    let server = Server::of(&work);
    let reply = server.get(KIT);
    assert_eq!(reply.status, 200);
    assert_eq!(reply.bytes, bytes(1, 3000));
    assert_eq!(header(&reply, "content-length"), "3000");
    assert_eq!(
        header(&reply, "etag"),
        tag(&work, "HEAD", "brand/archive/Kit 2026.zip")
    );
    assert_eq!(header(&reply, "accept-ranges"), "bytes");
    assert_eq!(header(&reply, "cache-control"), "no-cache");
    assert_eq!(header(&reply, "content-type"), "application/octet-stream");
    assert_eq!(
        header(&reply, "content-disposition"),
        r#"attachment; filename="Kit 2026.zip""#
    );
    assert_eq!(header(&reply, "x-content-type-options"), "nosniff");
    assert_eq!(header(&reply, "content-security-policy"), SANDBOXED);
    assert_eq!(reply.header("transfer-encoding"), None);
    assert!(
        server
            .log_with(r#""path":"/v1/raw/brand/archive/Kit%202026.zip""#)
            .iter()
            .any(|line| line.contains(r#""status":200"#)),
        "the request is logged like any other"
    );
}

#[test]
fn head_answers_with_the_headers_and_no_body() {
    let work = brand();
    let server = Server::of(&work);
    let got = server.get(KIT);
    let head = server.request("HEAD", KIT, &[("range", "bytes=0-9")], b"");
    assert_eq!(head.status, 200, "a range is for GET alone");
    assert_eq!(head.bytes, b"");
    for name in [
        "content-length",
        "etag",
        "accept-ranges",
        "cache-control",
        "content-type",
        "content-disposition",
        "x-content-type-options",
        "content-security-policy",
    ] {
        assert_eq!(head.header(name), got.header(name), "{name}");
    }
    assert_eq!(
        server
            .request("HEAD", "/v1/raw/nothing.zip", &[], b"")
            .status,
        404
    );
}

#[test]
fn only_what_cannot_run_in_a_browser_is_shown_in_place() {
    let work = brand();
    work.write(
        "site/page.html",
        "<!doctype html><script>document.title = 'ran'</script>\n",
    );
    work.write("site/feed.xml", "<?xml version=\"1.0\"?><feed/>\n");
    work.write("site/app.js", "document.title = 'ran';\n");
    work.write("site/style.css", "body { color: red }\n");
    work.write_bytes("brand/deck.pdf", b"%PDF-1.4\nNot a real deck.\n");
    work.write("data/table.csv", "name,colour\nink,#111111\n");
    work.write("data/settings.json", "{ \"paper\": \"#ffffff\" }\n");
    work.write_bytes("media/intro.mp4", &bytes(4, 64));
    work.write_bytes("media/voice.mp3", &bytes(5, 64));
    work.commit("Add a site, a deck, data and media");
    let server = Server::of(&work);
    let cases = [
        ("brand/logo.svg", "image/svg+xml", "inline", SANDBOXED),
        ("brand/mark.png", "image/png", "inline", SANDBOXED),
        (
            "brand/deck.pdf",
            "application/pdf",
            "inline",
            "default-src 'none'; style-src 'unsafe-inline'",
        ),
        ("media/intro.mp4", "video/mp4", "inline", SANDBOXED),
        ("media/voice.mp3", "audio/mpeg", "inline", SANDBOXED),
        (
            "brand/palette.txt",
            "text/plain; charset=utf-8",
            "inline",
            SANDBOXED,
        ),
        (
            "data/table.csv",
            "text/plain; charset=utf-8",
            "inline",
            SANDBOXED,
        ),
        (
            "data/settings.json",
            "text/plain; charset=utf-8",
            "inline",
            SANDBOXED,
        ),
        ("index.md", "text/plain; charset=utf-8", "inline", SANDBOXED),
        (
            "site/page.html",
            "application/octet-stream",
            "attachment",
            SANDBOXED,
        ),
        (
            "site/feed.xml",
            "application/octet-stream",
            "attachment",
            SANDBOXED,
        ),
        (
            "site/app.js",
            "application/octet-stream",
            "attachment",
            SANDBOXED,
        ),
        (
            "site/style.css",
            "application/octet-stream",
            "attachment",
            SANDBOXED,
        ),
    ];
    for (path, content_type, shown, policy) in cases {
        let reply = server.get(&format!("/v1/raw/{path}"));
        assert_eq!(reply.status, 200, "{path}");
        assert_eq!(header(&reply, "content-type"), content_type, "{path}");
        let name = path.rsplit('/').next().unwrap();
        assert_eq!(
            header(&reply, "content-disposition"),
            format!("{shown}; filename=\"{name}\""),
            "{path}"
        );
        assert_eq!(header(&reply, "content-security-policy"), policy, "{path}");
        assert_eq!(
            header(&reply, "x-content-type-options"),
            "nosniff",
            "{path}"
        );
        assert_eq!(
            reply.bytes,
            fs::read(work.file(path)).unwrap(),
            "{path} is sent as it is"
        );
    }
    let listed = server.json("/v1/files?under=site");
    for file in listed["files"].as_array().unwrap() {
        assert_eq!(
            file["type"], "application/octet-stream",
            "the listing names the type the bytes are sent with: {file}"
        );
    }
}

#[test]
fn a_name_with_spaces_and_letters_that_are_not_ascii_is_sent_encoded() {
    let work = brand();
    let server = Server::of(&work);
    let reply = server.get("/v1/raw/brand/%C3%85rets%20logga%202026.png");
    assert_eq!(reply.status, 200);
    assert_eq!(reply.bytes, bytes(2, 700));
    assert_eq!(header(&reply, "content-type"), "image/png");
    assert_eq!(
        header(&reply, "content-disposition"),
        "inline; filename=\"_rets logga 2026.png\"; \
         filename*=UTF-8''%C3%85rets%20logga%202026.png"
    );
}

#[test]
fn a_known_tag_gets_304_and_another_gets_the_file() {
    let work = brand();
    let server = Server::of(&work);
    let known = tag(&work, "HEAD", "brand/logo.svg");
    let fresh = server.get_with("/v1/raw/brand/logo.svg", &[("if-none-match", &known)]);
    assert_eq!(fresh.status, 304);
    assert_eq!(fresh.bytes, b"");
    assert_eq!(header(&fresh, "etag"), known);
    assert_eq!(header(&fresh, "cache-control"), "no-cache");
    let listed = format!("\"other\", W/{known}");
    let fresh = server.get_with("/v1/raw/brand/logo.svg", &[("if-none-match", &listed)]);
    assert_eq!(fresh.status, 304);
    let old = tag(&work, "HEAD~4", "brand/logo.svg");
    assert_ne!(old, known);
    let changed = server.get_with("/v1/raw/brand/logo.svg", &[("if-none-match", &old)]);
    assert_eq!(changed.status, 200);
    assert_eq!(changed.bytes.len(), 62);
    let head = server.request(
        "HEAD",
        "/v1/raw/brand/logo.svg",
        &[("if-none-match", &known)],
        b"",
    );
    assert_eq!(head.status, 304);
}

/// A cache takes the headers of a 304 into the answer it keeps, so a 304 carries the file's own
/// policy: a PDF must not gain a sandbox, nor an SVG lose its inline styles.
#[test]
fn a_304_carries_the_files_own_policy() {
    let work = brand();
    work.write_bytes("brand/deck.pdf", b"%PDF-1.4\nNot a real deck.\n");
    work.commit("Add a deck");
    let server = Server::of(&work);
    for (path, policy) in [
        ("brand/logo.svg", SANDBOXED),
        (
            "brand/deck.pdf",
            "default-src 'none'; style-src 'unsafe-inline'",
        ),
    ] {
        let url = format!("/v1/raw/{path}");
        let first = server.get(&url);
        assert_eq!(header(&first, "content-security-policy"), policy, "{path}");
        let known = header(&first, "etag").to_owned();
        let again = server.get_with(&url, &[("if-none-match", &known)]);
        assert_eq!(again.status, 304, "{path}");
        assert_eq!(header(&again, "content-security-policy"), policy, "{path}");
        assert_eq!(
            header(&again, "x-content-type-options"),
            "nosniff",
            "{path}"
        );
    }
}

#[test]
fn one_range_is_sent_as_206_and_one_that_cannot_be_met_is_416() {
    let work = brand();
    let server = Server::of(&work);
    let whole = bytes(1, 3000);
    let known = tag(&work, "HEAD", "brand/archive/Kit 2026.zip");
    for (range, first, last) in [
        ("bytes=0-0", 0, 0),
        ("bytes=1000-1999", 1000, 1999),
        ("bytes=2990-", 2990, 2999),
        ("bytes=2990-99999", 2990, 2999),
        ("bytes=-10", 2990, 2999),
        ("bytes=-99999", 0, 2999),
    ] {
        let reply = server.get_with(KIT, &[("range", range)]);
        assert_eq!(reply.status, 206, "{range}");
        assert_eq!(
            header(&reply, "content-range"),
            format!("bytes {first}-{last}/3000"),
            "{range}"
        );
        assert_eq!(
            header(&reply, "content-length"),
            (last - first + 1).to_string(),
            "{range}"
        );
        assert_eq!(reply.bytes, whole[first..=last], "{range}");
        assert_eq!(header(&reply, "etag"), known, "{range}");
        assert_eq!(header(&reply, "accept-ranges"), "bytes", "{range}");
        assert_eq!(header(&reply, "content-type"), "application/octet-stream");
        assert_eq!(header(&reply, "content-security-policy"), SANDBOXED);
    }
    for range in ["bytes=3000-", "bytes=3000-3010", "bytes=-0"] {
        let reply = server.get_with(KIT, &[("range", range)]);
        assert_eq!(reply.status, 416, "{range}");
        assert_eq!(header(&reply, "content-range"), "bytes */3000", "{range}");
        assert_eq!(
            reply.json()["error"],
            "the file has no byte in this range",
            "{range}"
        );
        assert_eq!(header(&reply, "x-content-type-options"), "nosniff");
    }
    for range in ["bytes=0-9,20-29", "bytes=9-0", "lines=0-9", "bytes=a-b"] {
        let reply = server.get_with(KIT, &[("range", range)]);
        assert_eq!(reply.status, 200, "{range} is answered with the whole file");
        assert_eq!(reply.bytes, whole, "{range}");
        assert_eq!(reply.header("content-range"), None, "{range}");
    }
}

#[test]
fn if_range_gives_the_range_only_of_the_bytes_the_client_has() {
    let work = brand();
    let server = Server::of(&work);
    let known = tag(&work, "HEAD", "brand/archive/Kit 2026.zip");
    let same = server.get_with(KIT, &[("range", "bytes=10-19"), ("if-range", &known)]);
    assert_eq!(same.status, 206);
    assert_eq!(same.bytes, bytes(1, 3000)[10..20]);
    for other in [
        "\"0000000000000000000000000000000000000000\"",
        "Sat, 14 Nov 2026 10:00:00 GMT",
    ] {
        let reply = server.get_with(KIT, &[("range", "bytes=10-19"), ("if-range", other)]);
        assert_eq!(reply.status, 200, "{other}");
        assert_eq!(reply.bytes.len(), 3000, "{other}");
    }
}

#[test]
fn a_moved_and_a_deleted_file_are_still_there_at_a_commit() {
    let work = brand();
    let server = Server::of(&work);
    let commits: Vec<String> = work
        .ok(&["log", "--reverse", "--format=%H"])
        .lines()
        .map(str::to_owned)
        .collect();
    let moved = server.get("/v1/raw/brand/kit.zip");
    assert_eq!(moved.status, 404);
    assert_eq!(moved.json()["error"], "no file is at this path");
    assert_eq!(header(&moved, "x-content-type-options"), "nosniff");
    assert_eq!(
        header(&moved, "content-security-policy"),
        "default-src 'none'; sandbox"
    );
    let then = server.get(&format!("/v1/raw/brand/kit.zip?at={}", commits[1]));
    assert_eq!(then.status, 200);
    assert_eq!(then.bytes, bytes(1, 3000));
    assert_eq!(
        header(&then, "cache-control"),
        "max-age=31536000, immutable",
        "the bytes at a commit never change"
    );
    assert_eq!(
        header(&then, "etag"),
        tag(&work, &commits[1], "brand/kit.zip")
    );
    let later = server.get(&format!("/v1/raw/brand/kit.zip?at={}", commits[2]));
    assert_eq!(later.status, 404, "the file had moved by then");
    assert_eq!(
        later.json()["error"],
        "no file was at this path at this commit"
    );

    assert_eq!(server.get("/v1/raw/notes/readme.txt").status, 404);
    let deleted = server.get(&format!("/v1/raw/notes/readme.txt?at={}", commits[2]));
    assert_eq!(deleted.status, 200);
    assert_eq!(deleted.body, "Not a page.\n");

    let first = server.get(&format!("/v1/raw/brand/logo.svg?at={}", commits[0]));
    assert!(first.body.contains("r=\"4\""), "{}", first.body);
    let now = server.get("/v1/raw/brand/logo.svg");
    assert!(now.body.contains("r=\"5\""), "{}", now.body);
    let range = server.get_with(
        &format!("/v1/raw/brand/logo.svg?at={}", commits[0]),
        &[("range", "bytes=1-3")],
    );
    assert_eq!((range.status, range.body.as_str()), (206, "svg"));
    let page = server.get(&format!("/v1/raw/index.md?at={}", commits[0]));
    assert_eq!(
        page.body,
        "# Home\n\nThe brand kit is in [brand](brand/).\n"
    );
}

#[test]
fn a_commit_that_is_unknown_or_not_on_main_is_404_and_a_short_one_is_400() {
    let work = brand();
    work.ok(&["checkout", "-q", "-b", "aside"]);
    work.write("brand/aside.txt", "Only on a branch.\n");
    let aside = work.commit("A commit only on a branch");
    work.ok(&["checkout", "-q", "main"]);
    let server = Server::of(&work);
    let head = work.ok(&["rev-parse", "HEAD"]);
    let head = head.trim();
    for (at, status, error) in [
        (aside.as_str(), 404, "no commit on main has this id"),
        (&"0".repeat(40), 404, "no commit on main has this id"),
        (&head[..12], 400, "at is not a full commit id"),
        ("main", 400, "at is not a full commit id"),
        ("", 400, "at is not a full commit id"),
    ] {
        let reply = server.get(&format!("/v1/raw/brand/logo.svg?at={at}"));
        assert_eq!(reply.status, status, "{at}");
        assert_eq!(reply.json()["error"], error, "{at}");
    }
    let upper = server.get(&format!(
        "/v1/raw/brand/logo.svg?at={}",
        head.to_ascii_uppercase()
    ));
    assert_eq!(upper.status, 200, "a commit id is read without case");
    for path in [
        "brand",
        "brand/",
        "brand//logo.svg",
        "brand/../index.md",
        "",
    ] {
        let reply = server.get(&format!("/v1/raw/{path}?at={head}"));
        assert_eq!(reply.status, 404, "{path:?}");
    }
}

#[test]
fn a_symlink_a_folder_and_a_submodule_have_no_bytes() {
    let work = Work::new();
    work.write("real.txt", "A real file.\n");
    std::os::unix::fs::symlink("real.txt", work.file("link.txt")).unwrap();
    let nested = work.file("vendor/lib");
    fs::create_dir_all(&nested).unwrap();
    work.git.ok(&nested, &["init", "-q", "-b", "main"]);
    fs::write(nested.join("lib.txt"), "Inside a repository of its own.\n").unwrap();
    work.git.ok(&nested, &["add", "-A"]);
    work.git.ok(&nested, &["commit", "-q", "-m", "Start"]);
    fs::write(work.file("run.sh"), "#!/bin/sh\necho ran\n").unwrap();
    work.ok(&["add", "-A"]);
    work.ok(&["update-index", "--chmod=+x", "run.sh"]);
    work.commit("A file, a symlink, a submodule and a script");
    let server = Server::of(&work);
    assert_eq!(server.get("/v1/raw/real.txt").body, "A real file.\n");
    let script = server.get("/v1/raw/run.sh");
    assert_eq!(
        script.body, "#!/bin/sh\necho ran\n",
        "an executable is a file"
    );
    assert_eq!(header(&script, "content-type"), "application/octet-stream");
    for path in ["link.txt", "vendor", "vendor/lib", "vendor/lib/lib.txt"] {
        assert_eq!(server.get(&format!("/v1/raw/{path}")).status, 404, "{path}");
    }
}

#[test]
fn a_private_api_asks_for_a_token_and_keeps_the_bytes_out_of_shared_caches() {
    let work = brand();
    let options = Options {
        private_api: true,
        ..Options::default()
    };
    let server = Server::of_with(&work, &options);
    for path in [
        KIT,
        "/v1/files",
        "/v1/files/brand/logo.svg",
        "/v1/changes/files",
    ] {
        let refused = server.get(path);
        assert_eq!(refused.status, 401, "{path}");
        assert_eq!(refused.bytes, b"", "{path}");
    }
    let authorization = [(
        "authorization",
        AUTHORIZATION.trim_start_matches("Authorization: "),
    )];
    let reply = server.get_with(KIT, &authorization);
    assert_eq!(reply.status, 200);
    assert_eq!(header(&reply, "cache-control"), "private, no-cache");
    let head = work.ok(&["rev-parse", "HEAD"]);
    let then = server.get_with(&format!("{KIT}?at={}", head.trim()), &authorization);
    assert_eq!(
        header(&then, "cache-control"),
        "private, max-age=31536000, immutable"
    );
    assert_eq!(server.get_with("/v1/files", &authorization).status, 200);
}

/// A file large enough to be sent in many pieces, read whole and by range, as Git stores it
/// loose, in a pack, and as a delta of an earlier version.
#[test]
fn a_file_is_read_from_a_loose_object_a_pack_and_a_delta() {
    let work = Work::new();
    let first = bytes(6, 700_000);
    let mut second = first.clone();
    second[350_000..350_010].copy_from_slice(b"new bytes!");
    work.write_bytes("brand/film.bin", &first);
    work.write(
        "notes.txt",
        &"A line of text that is here many times over.\n".repeat(4000),
    );
    let old = work.commit("Add a film and notes");
    work.write_bytes("brand/film.bin", &second);
    work.write(
        "notes.txt",
        &"A line of text that is here many times over.\n".repeat(4001),
    );
    work.commit("Change both");
    let check = |server: &Server, stored: &str| {
        let whole = server.get("/v1/raw/brand/film.bin");
        assert_eq!(header(&whole, "content-length"), "700000", "{stored}");
        assert!(whole.bytes == second, "the new film, {stored}");
        let then = server.get(&format!("/v1/raw/brand/film.bin?at={old}"));
        assert!(then.bytes == first, "the old film, {stored}");
        let middle = server.get_with(
            "/v1/raw/brand/film.bin",
            &[("range", "bytes=349990-350019")],
        );
        assert_eq!(middle.status, 206, "{stored}");
        assert_eq!(middle.bytes, second[349_990..350_020], "{stored}");
        let end = server.get_with("/v1/raw/brand/film.bin", &[("range", "bytes=-5")]);
        assert_eq!(end.bytes, second[699_995..], "{stored}");
        let notes = server.get(&format!("/v1/raw/notes.txt?at={old}"));
        assert_eq!(notes.bytes.len(), 45 * 4000, "{stored}");
        assert_eq!(
            server.get("/v1/raw/notes.txt").bytes.len(),
            45 * 4001,
            "{stored}"
        );
    };
    // How Git stores the version of `path` in `commit`: its size on disk, and what it is a
    // delta of.
    let stored = |commit: &str, path: &str| {
        let id = work.ok(&["rev-parse", &format!("{commit}:{path}")]);
        work.git_stdin(
            &["cat-file", "--batch-check=%(objectsize:disk) %(deltabase)"],
            &id,
        )
    };
    assert!(
        work.file(".git/objects/pack")
            .read_dir()
            .unwrap()
            .next()
            .is_none(),
        "every object is loose"
    );
    check(&Server::of(&work), "loose");

    work.ok(&["gc", "-q", "--aggressive"]);
    let zeros = "0".repeat(40);
    let whole = |commit: &str, path: &str| stored(commit, path).trim().ends_with(&zeros);
    assert!(
        !whole("HEAD", "brand/film.bin") || !whole(&old, "brand/film.bin"),
        "Git stored one version of the film as a delta of the other"
    );
    check(&Server::of(&work), "packed with deltas");

    work.ok(&[
        "-c",
        "core.bigFileThreshold=100k",
        "repack",
        "-q",
        "-a",
        "-d",
        "-F",
    ]);
    assert!(
        whole("HEAD", "brand/film.bin") && whole(&old, "brand/film.bin"),
        "Git stored both versions of the film whole"
    );
    check(&Server::of(&work), "packed whole");
}

/// A stock `git gc` stores one version of a large file as a delta of the other, which can only be
/// read whole. The server refuses to hold one over [`MAX_WHOLE`] in memory and says why, while a
/// folder, the user's own clone, reads it whole.
#[test]
fn a_large_file_stored_as_a_delta_is_refused_by_the_server_and_read_whole_in_a_folder() {
    let block = bytes(12, 4096);
    let film = |tail: &str| -> Vec<u8> {
        let mut film: Vec<u8> = block
            .iter()
            .copied()
            .cycle()
            .take(usize::try_from(MAX_WHOLE).unwrap() + 4096)
            .collect();
        film.extend_from_slice(tail.as_bytes());
        film
    };
    let work = Work::new();
    let (first, second) = (film("the first cut"), film("the second cut"));
    work.write_bytes("brand/film.bin", &first);
    let old = work.commit("Add a film");
    work.write_bytes("brand/film.bin", &second);
    let new = work.commit("Cut it again");
    work.ok(&["gc", "-q", "--aggressive"]);
    let delta = [(&old, &first), (&new, &second)]
        .into_iter()
        .find(|(commit, _)| {
            let id = work.ok(&["rev-parse", &format!("{commit}:brand/film.bin")]);
            let base = work.git_stdin(&["cat-file", "--batch-check=%(deltabase)"], &id);
            base.trim().bytes().any(|b| b != b'0')
        })
        .expect("Git stored one version as a delta of the other");
    let (commit, bytes_of_delta) = delta;
    let blob = work.ok(&["rev-parse", &format!("{commit}:brand/film.bin")]);
    let url = format!("/v1/raw/brand/film.bin?at={commit}");

    let server = Server::of(&work);
    let refused = server.get(&url);
    assert_eq!(refused.status, 503, "{}", refused.body);
    assert!(
        refused.json()["error"]
            .as_str()
            .unwrap()
            .contains("stored as a delta of another and is over 10 MiB"),
        "{}",
        refused.body
    );
    assert_eq!(header(&refused, "x-content-type-options"), "nosniff");
    assert_eq!(refused.header("etag"), None);
    let log = server.log_with(r#""path":"brand/film.bin""#);
    assert!(
        log.iter()
            .any(|line| line.contains(r#""event":"error""#) && line.contains(blob.trim())),
        "the log names the blob: {log:?}"
    );
    let head = server.request("HEAD", &url, &[], b"");
    assert_eq!(
        head.status, 200,
        "HEAD reads no bytes, so it needs none whole"
    );
    let ranged = server.get_with(&url, &[("range", "bytes=0-9")]);
    assert_eq!(ranged.status, 503, "a range would read it whole as well");

    let folder = TempDir::new().unwrap();
    work.ok(&["clone", "-q", ".", folder.path().to_str().unwrap()]);
    let local = Server::folder(folder.path(), &work.git, &[]);
    let read = local.get(&url);
    assert_eq!(read.status, 200, "{}", read.body);
    assert!(
        &read.bytes == bytes_of_delta,
        "the folder sends the whole file"
    );
}

#[test]
fn a_folder_sends_the_same_files_as_the_server() {
    let work = brand();
    let folder = TempDir::new().unwrap();
    work.ok(&["clone", "-q", ".", folder.path().to_str().unwrap()]);
    let server = Server::folder(folder.path(), &work.git, &[]);
    let of_repo = Server::of(&work);
    for route in [
        "/v1/files",
        "/v1/files?under=brand&sort=size",
        "/v1/files/brand/archive/Kit%202026.zip",
        "/v1/files/notes/readme.txt",
        "/v1/changes/files",
    ] {
        assert_eq!(server.json(route), of_repo.json(route), "{route}");
    }
    let (here, there) = (server.get(KIT), of_repo.get(KIT));
    assert_eq!(here.status, 200);
    assert_eq!(here.bytes, there.bytes);
    assert_eq!(here.headers.len(), there.headers.len());
    for (name, value) in &here.headers {
        if name != "date" {
            assert_eq!(there.header(name), Some(value.as_str()), "{name}");
        }
    }
    let away = server.get_as(Some("example.com"), KIT);
    assert_eq!(away.status, 421, "the bytes answer only to a loopback name");
}
