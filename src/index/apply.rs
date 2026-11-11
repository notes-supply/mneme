//! Writes one commit's changes to the index.

use anyhow::{Context, Result, bail};
use gix::ObjectId;
use rusqlite::{OptionalExtension, Transaction, params};

use super::PageId;
use super::search::MARKERS;
use crate::markdown;
use crate::repo::{Change, Commit, Repo};
use crate::text::{self, Churn};

/// What the index keeps of one blob's text.
struct PageText {
    title: String,
    words: u32,
    body: String,
    markdown: String,
}

/// The live page at a path, as it was before the commit being applied.
struct Live {
    rowid: i64,
    words: u32,
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
