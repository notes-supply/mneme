//! The in-memory SQLite index of pages, their versions and their text. All SQL lives here.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use gix::ObjectId;
use rusqlite::Connection;
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::repo::Repo;

mod apply;
mod read;
mod search;

pub use read::{Diff, End, Filter, Listing, Missing, Order, PathMatch, Question, Sort, Window};
pub use search::{Hit, Mark, Query, QueryError};

/// Stable id of a page across renames: a hash of the path it was created at and the commit
/// that created it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct PageId(String);

impl PageId {
    pub(crate) fn new(commit: &ObjectId, path: &str) -> Self {
        let digest = Sha256::new()
            .chain_update(commit.as_bytes())
            .chain_update(path.as_bytes())
            .finalize();
        Self(crate::hex(&digest[..8]))
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
    /// Open questions on the page now.
    pub questions: u32,
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

pub(super) const PAGE_COLUMNS: &str = "p.id, p.path, p.title, p.words, p.created, p.updated, p.versions, p.churn, p.questions, \
     p.deleted";

pub(super) const VERSION_SELECT: &str = "SELECT c.id, p.id, v.path, c.time, c.author, v.kind, v.from_path, \
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
            apply::apply(&tx, repo, commit)
                .with_context(|| format!("indexing commit {}", commit.id))?;
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
}

pub(super) fn page_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Page> {
    Ok(Page {
        id: PageId(row.get(0)?),
        path: row.get(1)?,
        title: row.get(2)?,
        words: row.get(3)?,
        created: row.get(4)?,
        updated: row.get(5)?,
        versions: row.get(6)?,
        churn: row.get(7)?,
        questions: row.get(8)?,
        deleted: row.get(9)?,
    })
}

pub(super) fn version_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Version> {
    let kind = match row.get_ref(5)?.as_str()? {
        "created" => Kind::Created,
        "edited" => Kind::Edited,
        "renamed" => Kind::Renamed { from: row.get(6)? },
        "deleted" => Kind::Deleted,
        other => {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                5,
                rusqlite::types::Type::Text,
                format!("unknown kind {other}").into(),
            ));
        }
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
