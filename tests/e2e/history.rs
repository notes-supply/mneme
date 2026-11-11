use mneme::index::{Index, Kind, Page};
use mneme::repo::Repo;

use crate::git::{self, Work};

fn index(work: &Work) -> Index {
    Index::build(&Repo::open(work.path()).unwrap()).unwrap()
}

fn page<'a>(pages: &'a [Page], path: &str) -> &'a Page {
    pages
        .iter()
        .find(|page| page.path == path)
        .unwrap_or_else(|| panic!("no page at {path}"))
}

#[test]
fn every_page_has_as_many_versions_as_git_log_follow_finds() {
    let work = git::standard();
    let index = index(&work);
    let pages = index.pages(true).unwrap();
    assert_eq!(pages.len(), 6, "{pages:#?}");
    for page in &pages {
        let events = index.versions(&page.id).unwrap();
        let (last, created) = (events.first().unwrap(), events.last().unwrap());
        let expected = work.text_changes(&page.path, &created.commit, &last.commit);
        assert_eq!(page.versions as usize, expected, "{}", page.path);
        let versions = events.iter().filter(|event| event.version).count();
        assert_eq!(versions, expected, "{}", page.path);
    }
}

#[test]
fn a_rename_keeps_the_page_and_is_not_a_version() {
    let work = git::standard();
    let index = index(&work);
    let pages = index.pages(false).unwrap();
    let alpha = page(&pages, "greek/alpha.md");
    let events = index.versions(&alpha.id).unwrap();
    let kinds: Vec<_> = events.iter().map(|e| (&e.kind, e.version)).collect();
    assert_eq!(
        kinds,
        [
            (&Kind::Edited, true),
            (
                &Kind::Renamed {
                    from: "alpha.md".to_owned()
                },
                false
            ),
            (&Kind::Edited, true),
            (&Kind::Created, true),
        ]
    );
    assert_eq!(alpha.versions, 3);
}

#[test]
fn a_rename_with_an_edit_is_a_version() {
    let work = git::standard();
    let index = index(&work);
    let pages = index.pages(false).unwrap();
    let memories = page(&pages, "minnen från gotland.md");
    let events = index.versions(&memories.id).unwrap();
    let renamed = &events[0];
    assert_eq!(
        renamed.kind,
        Kind::Renamed {
            from: "gotländska minnen.md".to_owned()
        }
    );
    assert!(renamed.version);
    assert_eq!((renamed.added, renamed.removed), (1, 1));
    assert_eq!(memories.title, "Gotländska minnen");
}

#[test]
fn a_page_recreated_at_the_same_path_is_a_new_page() {
    let work = git::standard();
    let index = index(&work);
    let pages = index.pages(true).unwrap();
    let deep: Vec<_> = pages.iter().filter(|p| p.path == "notes/deep.md").collect();
    assert_eq!(deep.len(), 2);
    assert_ne!(deep[0].id, deep[1].id);
    let (old, new) = if deep[0].deleted.is_some() {
        (deep[0], deep[1])
    } else {
        (deep[1], deep[0])
    };
    assert_eq!(
        old.title, "deep",
        "a page without a heading is titled by its file name"
    );
    assert_eq!(new.title, "Deep again");
    assert_eq!(new.deleted, None);
    let deletion = &index.versions(&old.id).unwrap()[0];
    assert_eq!(deletion.kind, Kind::Deleted);
    assert!(!deletion.version);
    assert_eq!(deletion.removed, 5);
}

#[test]
fn files_that_are_not_pages_are_ignored_and_dotted_names_are_kept() {
    let work = git::standard();
    let index = index(&work);
    let paths: Vec<_> = index
        .pages(false)
        .unwrap()
        .into_iter()
        .map(|page| page.path)
        .collect();
    assert_eq!(
        paths,
        [
            "AGENTS.md",
            "greek/alpha.md",
            "index.md",
            "minnen från gotland.md",
            "notes/deep.md"
        ]
    );
}

#[test]
fn the_overlap_trailer_and_the_author_are_recorded() {
    let work = git::standard();
    let index = index(&work);
    let pages = index.pages(false).unwrap();
    let agents = index.versions(&page(&pages, "AGENTS.md").id).unwrap();
    assert_eq!(agents[0].overlap.as_deref(), Some("aeolus"));
    assert_eq!(agents[1].overlap, None);
    assert_eq!(agents[0].author, "Writer");
    assert_eq!(
        agents[0].time,
        git::START + 8 * 600,
        "author time of the eighth commit"
    );
}

#[test]
fn an_update_with_new_commits_matches_a_fresh_build() {
    let work = git::standard();
    let repo = Repo::open(work.path()).unwrap();
    let mut index = Index::build(&repo).unwrap();
    work.rename("index.md", "home.md");
    work.write(
        "notes/deep.md",
        "# Deep again\n\nA new page at the old path, edited.\n",
    );
    work.commit("Rename home and edit the deep note");
    assert_eq!(index.update(&repo).unwrap(), 1);
    assert_eq!(index.update(&repo).unwrap(), 0);
    let fresh = Index::build(&repo).unwrap();
    let facts = |index: &Index| {
        index
            .pages(true)
            .unwrap()
            .into_iter()
            .map(|page| {
                let events = index.versions(&page.id).unwrap();
                format!("{page:?} {events:?}")
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(facts(&index), facts(&fresh));
    assert_eq!(index.status().unwrap().commits, 11);
}
