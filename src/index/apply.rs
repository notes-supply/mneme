//! Writes one commit's changes to the index.

use anyhow::{Context, Result, bail};
use gix::ObjectId;
use rusqlite::{OptionalExtension, Transaction, params};

use super::PageId;
use super::search::MARKERS;
use crate::markdown;
use crate::repo::{Change, Commit, FileChange, Repo};
use crate::text::{self, Churn};

/// What the index keeps of one blob's text.
struct PageText {
    title: String,
    words: u32,
    body: String,
    markdown: String,
    questions: Vec<String>,
}

/// The live page at a path, as it was before the commit being applied.
struct Live {
    rowid: i64,
    words: u32,
}

/// The live file at a path, as it was before the commit being applied.
struct LiveFile {
    rowid: i64,
    size: u64,
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
        title: title.replace(MARKERS, " "),
        body: visible.text.replace(MARKERS, " "),
        markdown,
        questions: visible.questions,
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
pub(super) fn apply(tx: &Transaction<'_>, repo: &Repo, commit: &Commit) -> Result<()> {
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
    step.files()
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
            "INSERT INTO pages (id, path, title, words, created, updated, versions, churn, \
             questions, blob) VALUES (?1, ?2, ?3, ?4, ?5, ?5, 1, ?4, 0, ?6)",
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
        self.questions(rowid, &text.questions)?;
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
        self.questions(source.rowid, &text.questions)
    }

    /// Replaces the page's open questions with `questions`, in page order.
    fn questions(&self, rowid: i64, questions: &[String]) -> Result<()> {
        self.tx
            .execute("DELETE FROM questions WHERE page = ?1", [rowid])?;
        let mut insert = self
            .tx
            .prepare_cached("INSERT INTO questions (page, position, text) VALUES (?1, ?2, ?3)")?;
        for (position, question) in (0_i64..).zip(questions) {
            insert.execute(params![rowid, position, question])?;
        }
        self.tx.execute(
            "UPDATE pages SET questions = ?2 WHERE rowid = ?1",
            params![rowid, i64::try_from(questions.len())?],
        )?;
        Ok(())
    }

    /// Applies the commit's changes to files that are not pages. As for pages, every file a
    /// change starts from is looked up before any change is written.
    fn files(&self) -> Result<()> {
        let sources = self
            .commit
            .files
            .iter()
            .map(|change| match change {
                FileChange::Added { .. } => Ok(None),
                FileChange::Modified { path: from, .. }
                | FileChange::Renamed { from, .. }
                | FileChange::Deleted { path: from } => self.live_file(from).map(Some),
            })
            .collect::<Result<Vec<_>>>()?;
        for (change, source) in self.commit.files.iter().zip(sources) {
            match (change, source) {
                (FileChange::Added { path, size }, None) => {
                    self.tx.execute(
                        "INSERT INTO files (path, folded, size, created, versions, last) \
                         VALUES (?1, ?2, ?3, ?4, 1, 0)",
                        params![path, path.to_lowercase(), size, self.commit.time],
                    )?;
                    let file = self.tx.last_insert_rowid();
                    self.file_event(file, path, "created", None, *size)?;
                }
                (FileChange::Modified { path, size }, Some(file)) => {
                    self.tx.execute(
                        "UPDATE files SET size = ?2, versions = versions + 1 WHERE rowid = ?1",
                        params![file.rowid, size],
                    )?;
                    self.file_event(file.rowid, path, "edited", None, *size)?;
                }
                (FileChange::Renamed { from, path }, Some(file)) => {
                    self.tx.execute(
                        "UPDATE files SET path = ?2, folded = ?3 WHERE rowid = ?1",
                        params![file.rowid, path, path.to_lowercase()],
                    )?;
                    self.file_event(file.rowid, path, "renamed", Some(from), file.size)?;
                }
                (FileChange::Deleted { path }, Some(file)) => {
                    self.tx.execute(
                        "UPDATE files SET deleted = ?2 WHERE rowid = ?1",
                        params![file.rowid, self.commit.time],
                    )?;
                    self.file_event(file.rowid, path, "deleted", None, 0)?;
                }
                (change, _) => bail!("unexpected change {change:?}"),
            }
        }
        Ok(())
    }

    fn live_file(&self, path: &str) -> Result<LiveFile> {
        self.tx
            .query_row(
                "SELECT rowid, size FROM files WHERE path = ?1 AND deleted IS NULL",
                [path],
                |row| {
                    Ok(LiveFile {
                        rowid: row.get(0)?,
                        size: row.get(1)?,
                    })
                },
            )
            .optional()?
            .with_context(|| format!("no file at {path} before this commit"))
    }

    /// Records an event of a file, which is then the file's newest.
    fn file_event(
        &self,
        file: i64,
        path: &str,
        kind: &str,
        from: Option<&str>,
        size: u64,
    ) -> Result<()> {
        self.tx.execute(
            "INSERT INTO file_events (commit_seq, file, path, kind, from_path, size) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![self.seq, file, path, kind, from, size],
        )?;
        self.tx.execute(
            "UPDATE files SET last = ?2 WHERE rowid = ?1",
            params![file, self.tx.last_insert_rowid()],
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
        self.questions(source.rowid, &[])
    }
}
