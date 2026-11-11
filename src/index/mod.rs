//! The in-memory SQLite index of pages, their versions and their text. All SQL lives here.

use std::fmt::Write as _;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use gix::ObjectId;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::markdown;
use crate::repo::{Change, Commit, Repo};
use crate::text::{self, Churn};

/// Stable id of a page across renames: a hash of the path it was created at and the commit
/// that created it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct PageId(String);

impl PageId {
    fn new(commit: &ObjectId, path: &str) -> Self {
        let digest = Sha256::new()
            .chain_update(commit.as_bytes())
            .chain_update(path.as_bytes())
            .finalize();
        let mut id = String::with_capacity(16);
        for byte in &digest[..8] {
            let _ = write!(id, "{byte:02x}");
        }
        Self(id)
    }

    /// The id as written in URLs.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for PageId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

/// A Markdown file of the wiki, followed across renames.
#[derive(Clone, Debug, Serialize)]
pub struct Page {
    pub id: PageId,
    /// The current path, or the last one for a deleted page.
    pub path: String,
    /// The first level-1 heading, or the file name without `.md`.
    pub title: String,
    pub words: u32,
    /// Author time of the commit that created the page.
    pub created: i64,
    /// Author time of the last version.
    pub updated: i64,
    pub versions: u32,
    /// Words added plus words removed over all versions.
    pub churn: i64,
    /// Author time of the commit that deleted the page.
    pub deleted: Option<i64>,
}

/// What a commit did to a page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Kind {
    Created,
    Edited,
    Renamed { from: String },
    Deleted,
}

/// One event in a page's history. It is a version when it changed the page's text.
#[derive(Clone, Debug, Serialize)]
pub struct Version {
    pub commit: String,
    pub page: PageId,
    /// The page's path after the event.
    pub path: String,
    /// Author time.
    pub time: i64,
    pub author: String,
    #[serde(flatten)]
    pub kind: Kind,
    pub version: bool,
    /// Words in the page after the event.
    pub words: u32,
    pub added: u32,
    pub removed: u32,
    pub overlap: Option<String>,
}

/// The facts the status route reports.
#[derive(Debug, Serialize)]
pub struct Status {
    pub head: Option<String>,
    pub head_time: Option<i64>,
    pub pages: u32,
    pub commits: u32,
    /// How long the first build from the repository took, in milliseconds.
    pub build_ms: u64,
}

/// The index of one repository's `main`.
pub struct Index {
    db: Connection,
    head: Option<ObjectId>,
    build: Duration,
}

/// The live page at a path, as it was before the commit being applied.
struct Live {
    rowid: i64,
    words: u32,
}

/// What the index keeps of one blob's text.
struct PageText {
    title: String,
    words: u32,
    body: String,
    markdown: String,
}

const PAGE_COLUMNS: &str =
    "p.id, p.path, p.title, p.words, p.created, p.updated, p.versions, p.churn, p.deleted";

const VERSION_SELECT: &str = "SELECT c.id, p.id, v.path, c.time, c.author, v.kind, v.from_path, \
     v.version, v.words, v.added, v.removed, c.overlap \
     FROM versions v JOIN commits c ON c.seq = v.commit_seq JOIN pages p ON p.rowid = v.page";

impl Index {
    /// Builds the index of every commit on `main`.
    ///
    /// # Errors
    ///
    /// Fails when the repository cannot be read.
    pub fn build(repo: &Repo) -> Result<Self> {
        let start = Instant::now();
        let db = Connection::open_in_memory()?;
        db.execute_batch(include_str!("schema.sql"))?;
        let mut index = Self {
            db,
            head: None,
            build: Duration::ZERO,
        };
        index.update(repo)?;
        index.build = start.elapsed();
        Ok(index)
    }

    /// Adds the commits made on `main` since the last update. Returns how many there were.
    ///
    /// # Errors
    ///
    /// Fails when the repository cannot be read. The index is then left as it was.
    pub fn update(&mut self, repo: &Repo) -> Result<usize> {
        let commits = repo.commits_after(self.head)?;
        let Some(last) = commits.last().map(|commit| commit.id) else {
            return Ok(0);
        };
        let tx = self.db.transaction()?;
        for commit in &commits {
            apply(&tx, repo, commit).with_context(|| format!("indexing commit {}", commit.id))?;
        }
        tx.commit()?;
        self.head = Some(last);
        Ok(commits.len())
    }

    /// Head commit, page and commit counts and the build time.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn status(&self) -> Result<Status> {
        let (head, head_time, commits) = self.db.query_row(
            "SELECT (SELECT id FROM commits ORDER BY seq DESC LIMIT 1), \
             (SELECT time FROM commits ORDER BY seq DESC LIMIT 1), count(*) FROM commits",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let pages = self.db.query_row(
            "SELECT count(*) FROM pages WHERE deleted IS NULL",
            [],
            |row| row.get(0),
        )?;
        Ok(Status {
            head,
            head_time,
            pages,
            commits,
            build_ms: self.build.as_millis().try_into().unwrap_or(u64::MAX),
        })
    }

    /// The page with this id, deleted or not.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn page(&self, id: &PageId) -> Result<Option<Page>> {
        Ok(self
            .db
            .query_row(
                &format!("SELECT {PAGE_COLUMNS} FROM pages p WHERE p.id = ?1"),
                [id.as_str()],
                page_row,
            )
            .optional()?)
    }

    /// Every page, by path. Deleted pages only when `deleted` is set.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn pages(&self, deleted: bool) -> Result<Vec<Page>> {
        let mut statement = self.db.prepare_cached(&format!(
            "SELECT {PAGE_COLUMNS} FROM pages p WHERE ?1 OR p.deleted IS NULL ORDER BY p.path"
        ))?;
        let pages = statement
            .query_map([deleted], page_row)?
            .collect::<Result<_, _>>()?;
        Ok(pages)
    }

    /// The events of one page, newest first.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn versions(&self, page: &PageId) -> Result<Vec<Version>> {
        let mut statement = self.db.prepare_cached(&format!(
            "{VERSION_SELECT} WHERE p.id = ?1 ORDER BY v.seq DESC"
        ))?;
        let versions = statement
            .query_map([page.as_str()], version_row)?
            .collect::<Result<_, _>>()?;
        Ok(versions)
    }
}

fn page_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Page> {
    Ok(Page {
        id: PageId(row.get(0)?),
        path: row.get(1)?,
        title: row.get(2)?,
        words: row.get(3)?,
        created: row.get(4)?,
        updated: row.get(5)?,
        versions: row.get(6)?,
        churn: row.get(7)?,
        deleted: row.get(8)?,
    })
}

fn version_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Version> {
    let kind = match row.get_ref(5)?.as_str()? {
        "created" => Kind::Created,
        "edited" => Kind::Edited,
        "renamed" => Kind::Renamed { from: row.get(6)? },
        _ => Kind::Deleted,
    };
    Ok(Version {
        commit: row.get(0)?,
        page: PageId(row.get(1)?),
        path: row.get(2)?,
        time: row.get(3)?,
        author: row.get(4)?,
        kind,
        version: row.get(7)?,
        words: row.get(8)?,
        added: row.get(9)?,
        removed: row.get(10)?,
        overlap: row.get(11)?,
    })
}

fn read(repo: &Repo, path: &str, blob: ObjectId) -> Result<PageText> {
    let markdown = repo.text(blob)?;
    let visible = markdown::visible(&markdown);
    let title = visible.title.unwrap_or_else(|| {
        let name = path.rsplit('/').next().unwrap_or(path);
        name.strip_suffix(".md").unwrap_or(name).to_owned()
    });
    Ok(PageText {
        words: text::word_count(&markdown),
        title,
        body: visible.text,
        markdown,
    })
}

fn live(tx: &Transaction<'_>, path: &str) -> Result<Live> {
    tx.query_row(
        "SELECT rowid, words FROM pages WHERE path = ?1 AND deleted IS NULL",
        [path],
        |row| {
            Ok(Live {
                rowid: row.get(0)?,
                words: row.get(1)?,
            })
        },
    )
    .optional()?
    .with_context(|| format!("no page at {path} before this commit"))
}

/// Applies one commit. Every page a change starts from is looked up before any change is
/// written, so a chain of renames inside one commit finds the pages as they were.
fn apply(tx: &Transaction<'_>, repo: &Repo, commit: &Commit) -> Result<()> {
    tx.execute(
        "INSERT INTO commits (id, time, author, overlap) VALUES (?1, ?2, ?3, ?4)",
        params![
            commit.id.to_string(),
            commit.time,
            commit.author,
            commit.overlap
        ],
    )?;
    let step = Step {
        tx,
        repo,
        commit,
        seq: tx.last_insert_rowid(),
    };
    let sources = commit
        .changes
        .iter()
        .map(|change| match change {
            Change::Added { .. } => Ok(None),
            Change::Modified { path: from, .. }
            | Change::Renamed { from, .. }
            | Change::Deleted { path: from, .. } => live(tx, from).map(Some),
        })
        .collect::<Result<Vec<_>>>()?;
    for (change, source) in commit.changes.iter().zip(sources) {
        match (change, source) {
            (Change::Added { path, blob }, None) => step.create(path, *blob)?,
            (
                Change::Modified {
                    from_blob, blob, ..
                },
                Some(_),
            ) if from_blob == blob => {}
            (
                Change::Modified {
                    path,
                    from_blob,
                    blob,
                },
                Some(source),
            ) => step.edit(&source, None, path, *from_blob, *blob)?,
            (
                Change::Renamed {
                    from,
                    path,
                    from_blob,
                    blob,
                },
                Some(source),
            ) => step.edit(&source, Some(from), path, *from_blob, *blob)?,
            (Change::Deleted { path, .. }, Some(source)) => step.delete(&source, path)?,
            (change, _) => bail!("unexpected change {change:?}"),
        }
    }
    Ok(())
}

/// The commit being applied and where its rows go.
struct Step<'a> {
    tx: &'a Transaction<'a>,
    repo: &'a Repo,
    commit: &'a Commit,
    seq: i64,
}

impl Step<'_> {
    fn create(&self, path: &str, blob: ObjectId) -> Result<()> {
        let text = read(self.repo, path, blob)?;
        let id = PageId::new(&self.commit.id, path);
        self.tx.execute(
            "INSERT INTO pages (id, path, title, words, created, updated, versions, churn, blob) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?5, 1, ?4, ?6)",
            params![
                id.0,
                path,
                text.title,
                text.words,
                self.commit.time,
                blob.to_string()
            ],
        )?;
        let rowid = self.tx.last_insert_rowid();
        self.tx.execute(
            "INSERT INTO versions (commit_seq, page, path, kind, version, words, added, removed, \
             blob) VALUES (?1, ?2, ?3, 'created', 1, ?4, ?4, 0, ?5)",
            params![self.seq, rowid, path, text.words, blob.to_string()],
        )?;
        self.tx.execute(
            "INSERT INTO search (rowid, title, body) VALUES (?1, ?2, ?3)",
            params![rowid, text.title, text.body],
        )?;
        Ok(())
    }

    /// An edit, a rename, or both. It is a version when the blob changed.
    fn edit(
        &self,
        source: &Live,
        from: Option<&str>,
        path: &str,
        from_blob: ObjectId,
        blob: ObjectId,
    ) -> Result<()> {
        let text = read(self.repo, path, blob)?;
        let version = from_blob != blob;
        let churn = if version {
            text::churn(&self.repo.text(from_blob)?, &text.markdown)
        } else {
            Churn::default()
        };
        self.tx.execute(
            "UPDATE pages SET path = ?2, title = ?3, words = ?4, blob = ?5, \
             updated = CASE WHEN ?6 THEN ?7 ELSE updated END, versions = versions + ?6, \
             churn = churn + ?8 WHERE rowid = ?1",
            params![
                source.rowid,
                path,
                text.title,
                text.words,
                blob.to_string(),
                version,
                self.commit.time,
                i64::from(churn.added) + i64::from(churn.removed),
            ],
        )?;
        self.tx.execute(
            "INSERT INTO versions (commit_seq, page, path, kind, from_path, version, words, \
             added, removed, blob) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                self.seq,
                source.rowid,
                path,
                if from.is_some() { "renamed" } else { "edited" },
                from,
                version,
                text.words,
                churn.added,
                churn.removed,
                blob.to_string(),
            ],
        )?;
        self.tx.execute(
            "UPDATE search SET title = ?2, body = ?3 WHERE rowid = ?1",
            params![source.rowid, text.title, text.body],
        )?;
        Ok(())
    }

    fn delete(&self, source: &Live, path: &str) -> Result<()> {
        self.tx.execute(
            "UPDATE pages SET deleted = ?2, blob = NULL WHERE rowid = ?1",
            params![source.rowid, self.commit.time],
        )?;
        self.tx.execute(
            "INSERT INTO versions (commit_seq, page, path, kind, version, words, added, removed) \
             VALUES (?1, ?2, ?3, 'deleted', 0, 0, 0, ?4)",
            params![self.seq, source.rowid, path, source.words],
        )?;
        self.tx
            .execute("DELETE FROM search WHERE rowid = ?1", [source.rowid])?;
        Ok(())
    }
}
