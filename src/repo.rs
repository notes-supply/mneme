//! Reads the wiki's bare repository with gix: the commits of `main` and the pages they change.

use std::path::Path;

use anyhow::{Context, Result};
use gix::ObjectId;
use gix::bstr::BStr;
use gix::diff::Rewrites;
use gix::object::tree::diff::ChangeDetached;
use gix::objs::tree::EntryMode;

/// The branch that holds the wiki. The push check keeps every other ref out.
pub const MAIN: &str = "refs/heads/main";

/// A commit that `main` no longer holds, as when someone reset `main` by hand.
#[derive(Debug, thiserror::Error)]
#[error("commit {0} is not on {MAIN}")]
pub struct NotOnMain(pub ObjectId);

/// One commit of `main` and the pages it changed.
#[derive(Debug)]
pub struct Commit {
    pub id: ObjectId,
    /// Author time, in seconds since the Unix epoch.
    pub time: i64,
    pub author: String,
    /// The value of an `Overlap:` trailer, which the autosave service writes when it had to pick
    /// a side.
    pub overlap: Option<String>,
    pub changes: Vec<Change>,
}

/// A change to one page file. Renames come from gix's rewrite tracking.
#[derive(Debug, PartialEq, Eq)]
pub enum Change {
    Added {
        path: String,
        blob: ObjectId,
    },
    Modified {
        path: String,
        from_blob: ObjectId,
        blob: ObjectId,
    },
    Renamed {
        from: String,
        path: String,
        from_blob: ObjectId,
        blob: ObjectId,
    },
    Deleted {
        path: String,
        from_blob: ObjectId,
    },
}

/// The wiki's repository, opened on one thread.
pub struct Repo {
    repo: gix::Repository,
}

impl Repo {
    /// Opens the repository at `path`.
    ///
    /// # Errors
    ///
    /// Fails when `path` is not a Git repository.
    pub fn open(path: &Path) -> Result<Self> {
        let mut repo = gix::open(path)
            .with_context(|| format!("opening the repository at {}", path.display()))?;
        repo.object_cache_size_if_unset(4 << 20);
        Ok(Self { repo })
    }

    /// The commit at the tip of `main`, or `None` while the repository is empty.
    ///
    /// # Errors
    ///
    /// Fails when the ref cannot be read.
    pub fn head(&self) -> Result<Option<ObjectId>> {
        let reference = self.repo.try_find_reference(MAIN)?;
        Ok(reference
            .map(|mut reference| reference.peel_to_id())
            .transpose()?
            .map(gix::Id::detach))
    }

    /// The commits of `main` after `after`, oldest first, along the first parent.
    ///
    /// # Errors
    ///
    /// Fails when an object cannot be read, or with [`NotOnMain`] when `after` is not on `main`.
    pub fn commits_after(&self, after: Option<ObjectId>) -> Result<Vec<Commit>> {
        let repo = &self.repo;
        let mut ids = Vec::new();
        let mut next = self.head()?;
        while let Some(id) = next {
            if Some(id) == after {
                break;
            }
            ids.push(id);
            next = repo
                .find_commit(id)?
                .parent_ids()
                .next()
                .map(gix::Id::detach);
        }
        if let (Some(after), None) = (after, next) {
            return Err(NotOnMain(after).into());
        }
        ids.iter().rev().map(|&id| commit(repo, id)).collect()
    }

    /// The text of a blob. Bytes that are not UTF-8 are replaced.
    ///
    /// # Errors
    ///
    /// Fails when the blob cannot be read.
    pub fn text(&self, blob: ObjectId) -> Result<String> {
        let blob = self.repo.find_blob(blob)?;
        Ok(String::from_utf8_lossy(&blob.data).into_owned())
    }
}

fn commit(repo: &gix::Repository, id: ObjectId) -> Result<Commit> {
    let commit = repo.find_commit(id)?;
    let decoded = commit.decode()?;
    let author = decoded.author()?;
    let overlap = decoded
        .message_trailers()
        .find(|trailer| trailer.token.eq_ignore_ascii_case(b"Overlap"))
        .map(|trailer| trailer.value.to_string());
    let old_tree = match commit.parent_ids().next() {
        Some(parent) => Some(repo.find_commit(parent)?.tree()?),
        None => None,
    };
    let new_tree = commit.tree()?;
    let rewrites = Rewrites {
        percentage: Some(0.5),
        ..Rewrites::default()
    };
    let options = gix::diff::Options::default().with_rewrites(Some(rewrites));
    let changes = repo
        .diff_tree_to_tree(old_tree.as_ref(), &new_tree, options)?
        .into_iter()
        .filter_map(page_change)
        .collect();
    Ok(Commit {
        id,
        time: author.seconds(),
        author: author.name.to_string(),
        overlap,
        changes,
    })
}

/// Whether a file at `path` is a page. Other files are stored by Git and not indexed.
#[must_use]
pub fn is_page(path: &[u8]) -> bool {
    path.ends_with(b".md")
}

fn page(location: &BStr, mode: EntryMode) -> Option<String> {
    (mode.is_blob() && is_page(location)).then(|| location.to_string())
}

/// The change to page files, seen from what was a page before and what is a page after.
fn page_change(change: ChangeDetached) -> Option<Change> {
    let (before, after) = match change {
        ChangeDetached::Addition {
            location,
            entry_mode,
            id,
            ..
        } => (
            None,
            page(location.as_ref(), entry_mode).map(|path| (path, id)),
        ),
        ChangeDetached::Deletion {
            location,
            entry_mode,
            id,
            ..
        } => (
            page(location.as_ref(), entry_mode).map(|path| (path, id)),
            None,
        ),
        ChangeDetached::Modification {
            location,
            previous_entry_mode,
            previous_id,
            entry_mode,
            id,
        } => (
            page(location.as_ref(), previous_entry_mode).map(|path| (path, previous_id)),
            page(location.as_ref(), entry_mode).map(|path| (path, id)),
        ),
        ChangeDetached::Rewrite {
            source_location,
            source_entry_mode,
            source_id,
            entry_mode,
            location,
            id,
            copy,
            ..
        } => (
            (!copy)
                .then(|| page(source_location.as_ref(), source_entry_mode))
                .flatten()
                .map(|path| (path, source_id)),
            page(location.as_ref(), entry_mode).map(|path| (path, id)),
        ),
    };
    match (before, after) {
        (None, None) => None,
        (None, Some((path, blob))) => Some(Change::Added { path, blob }),
        (Some((path, from_blob)), None) => Some(Change::Deleted { path, from_blob }),
        (Some((from, from_blob)), Some((path, blob))) if from == path => Some(Change::Modified {
            path,
            from_blob,
            blob,
        }),
        (Some((from, from_blob)), Some((path, blob))) => Some(Change::Renamed {
            from,
            path,
            from_blob,
            blob,
        }),
    }
}
