//! Reads the wiki's bare repository with gix: the commits of `main`, and the pages and the other
//! files they change.

use std::collections::{HashMap, VecDeque};
use std::path::Path;

use anyhow::{Context, Result};
use gix::ObjectId;
use gix::bstr::{BStr, ByteSlice};
use gix::diff::Rewrites;
use gix::object::tree::diff::ChangeDetached;
use gix::objs::tree::EntryMode;

use crate::{blob, rules};

/// The branch that holds the wiki. The push check keeps every other ref out.
pub const MAIN: &str = "refs/heads/main";
/// The tree with nothing in it, which a commit with no parent is compared with.
pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
/// The id Git reads as no object at all, such as the old value of a ref that must not exist yet.
pub const NO_OBJECT: &str = "0000000000000000000000000000000000000000";

/// A commit that `main` no longer holds, as when someone reset `main` by hand.
#[derive(Debug, thiserror::Error)]
#[error("commit {0} is not on {MAIN}")]
pub struct NotOnMain(pub ObjectId);

/// One commit of `main`, the pages it changed and the other files it changed.
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
    pub files: Vec<FileChange>,
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

/// A change to one file that is not a page. Its bytes are never read: the size comes from the
/// blob's header, and a rename is the same blob at another path.
#[derive(Debug, PartialEq, Eq)]
pub enum FileChange {
    Added { path: String, size: u64 },
    Modified { path: String, size: u64 },
    Renamed { from: String, path: String },
    Deleted { path: String },
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
        // A page is found again under a new name by comparing what a commit took away with
        // what it added, files that are not pages among them. gix reads no blob over this size
        // for that. The repository's own setting is for Git, which the server tells to stream
        // every blob over a page's size, and what the index reads must not move with it.
        let options = gix::open::Options::default()
            .config_overrides([format!("core.bigFileThreshold={}", rules::MAX_WHOLE)]);
        let mut repo = gix::open_opts(path, options)
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

    /// The blob of the regular file at `path` in the tree of `commit`, or `None` when nothing is
    /// there or what is there is a folder, a symlink or a submodule.
    ///
    /// # Errors
    ///
    /// Fails when the commit or one of its trees cannot be read.
    pub fn file_at(&self, commit: ObjectId, path: &str) -> Result<Option<ObjectId>> {
        let mut tree = self.repo.find_commit(commit)?.tree()?;
        let mut parts = path.split('/').peekable();
        while let Some(part) = parts.next() {
            let Some((mode, id)) = tree
                .find_entry(part)
                .map(|entry| (entry.mode(), entry.object_id()))
            else {
                return Ok(None);
            };
            if parts.peek().is_none() {
                return Ok(mode.is_blob().then_some(id));
            }
            // A submodule holds a commit of another repository, which has no tree here.
            if !mode.is_tree() {
                return Ok(None);
            }
            tree = self.repo.find_tree(id)?;
        }
        Ok(None)
    }

    /// The size of a blob in bytes, from its header.
    ///
    /// # Errors
    ///
    /// Fails when the blob cannot be found.
    pub fn blob_size(&self, blob: ObjectId) -> Result<u64> {
        size(&self.repo, blob)
    }

    /// Opens a blob to read its bytes in order.
    ///
    /// # Errors
    ///
    /// Fails when the blob cannot be found or read.
    pub fn open_blob(&self, blob: ObjectId) -> Result<blob::Reader> {
        blob::Reader::open(&self.repo, blob)
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
    let mut changes = Vec::new();
    let mut files = Vec::new();
    let (mut gone, mut came) = (Vec::new(), Vec::new());
    for change in repo.diff_tree_to_tree(old_tree.as_ref(), &new_tree, options)? {
        let (before, after) = ends(change);
        let (page_before, file_before) = by_kind(before);
        let (page_after, file_after) = by_kind(after);
        changes.extend(page_change(page_before, page_after));
        match (file_before, file_after) {
            (Some(before), Some(after)) if before.path == after.path => {
                if before.blob != after.blob {
                    files.push(FileChange::Modified {
                        size: size(repo, after.blob)?,
                        path: after.path,
                    });
                }
            }
            (before, after) => {
                gone.extend(before);
                came.extend(after);
            }
        }
    }
    for moved in moves(gone, came) {
        files.push(match moved {
            Moved::Renamed { from, path } => FileChange::Renamed { from, path },
            Moved::Deleted(path) => FileChange::Deleted { path },
            Moved::Added(entry) => FileChange::Added {
                size: size(repo, entry.blob)?,
                path: entry.path,
            },
        });
    }
    Ok(Commit {
        id,
        time: author.seconds(),
        author: author.name.to_string(),
        overlap,
        changes,
        files,
    })
}

/// The size of a blob, from its header.
fn size(repo: &gix::Repository, blob: ObjectId) -> Result<u64> {
    Ok(repo.find_header(blob)?.size())
}

/// Whether a file at `path` is a page. Any other regular file is a file, which the index lists
/// without reading.
#[must_use]
pub fn is_page(path: &[u8]) -> bool {
    path.ends_with(b".md")
}

/// A regular file at a path in a tree.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Entry {
    path: String,
    blob: ObjectId,
    page: bool,
}

/// The regular file at `location`. A page's path that is not UTF-8 has its bad bytes replaced,
/// as it always had. Any other file's must be UTF-8, or it is left out: two names that differ
/// only in bad bytes would be one path once they were replaced, and a change to one would land
/// on the other.
fn entry(location: &BStr, mode: EntryMode, blob: ObjectId) -> Option<Entry> {
    if !mode.is_blob() {
        return None;
    }
    let page = is_page(location);
    let path = if page {
        location.to_string()
    } else {
        location.to_str().ok()?.to_owned()
    };
    Some(Entry { path, blob, page })
}

/// An entry as a page, or as a file that is not one.
fn by_kind(entry: Option<Entry>) -> (Option<Entry>, Option<Entry>) {
    match entry {
        Some(entry) if entry.page => (Some(entry), None),
        other => (None, other),
    }
}

/// The regular file a change took from a path and the one it left at a path, seen from the
/// path's side: an edit in place takes and leaves at the same path.
fn ends(change: ChangeDetached) -> (Option<Entry>, Option<Entry>) {
    match change {
        ChangeDetached::Addition {
            location,
            entry_mode,
            id,
            ..
        } => (None, entry(location.as_bstr(), entry_mode, id)),
        ChangeDetached::Deletion {
            location,
            entry_mode,
            id,
            ..
        } => (entry(location.as_bstr(), entry_mode, id), None),
        ChangeDetached::Modification {
            location,
            previous_entry_mode,
            previous_id,
            entry_mode,
            id,
        } => (
            entry(location.as_bstr(), previous_entry_mode, previous_id),
            entry(location.as_bstr(), entry_mode, id),
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
                .then(|| entry(source_location.as_bstr(), source_entry_mode, source_id))
                .flatten(),
            entry(location.as_bstr(), entry_mode, id),
        ),
    }
}

/// The change to page files, seen from what was a page before and what is a page after.
fn page_change(before: Option<Entry>, after: Option<Entry>) -> Option<Change> {
    match (before, after) {
        (None, None) => None,
        (None, Some(after)) => Some(Change::Added {
            path: after.path,
            blob: after.blob,
        }),
        (Some(before), None) => Some(Change::Deleted {
            path: before.path,
            from_blob: before.blob,
        }),
        (Some(before), Some(after)) if before.path == after.path => Some(Change::Modified {
            path: after.path,
            from_blob: before.blob,
            blob: after.blob,
        }),
        (Some(before), Some(after)) => Some(Change::Renamed {
            from: before.path,
            path: after.path,
            from_blob: before.blob,
            blob: after.blob,
        }),
    }
}

/// What became of a file that left its path or came to one.
#[derive(Debug, PartialEq, Eq)]
enum Moved {
    Renamed { from: String, path: String },
    Deleted(String),
    Added(Entry),
}

/// Pairs the files a commit took from their paths with the files it left at new ones. A file
/// is renamed when the same blob is gone from one path and has come to another, and nothing
/// else is: no file is compared with another. Several files with the same bytes pair in path
/// order.
fn moves(mut gone: Vec<Entry>, mut came: Vec<Entry>) -> Vec<Moved> {
    gone.sort_by(|a, b| a.path.cmp(&b.path));
    came.sort_by(|a, b| a.path.cmp(&b.path));
    let mut sources: HashMap<ObjectId, VecDeque<String>> = HashMap::new();
    let mut moved = Vec::with_capacity(gone.len() + came.len());
    for entry in gone {
        sources.entry(entry.blob).or_default().push_back(entry.path);
    }
    for entry in came {
        let from = sources.get_mut(&entry.blob).and_then(VecDeque::pop_front);
        moved.push(match from {
            Some(from) => Moved::Renamed {
                from,
                path: entry.path,
            },
            None => Moved::Added(entry),
        });
    }
    let mut deleted: Vec<String> = sources.into_values().flatten().collect();
    deleted.sort();
    moved.extend(deleted.into_iter().map(Moved::Deleted));
    moved
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, blob: u8) -> Entry {
        Entry {
            path: path.to_owned(),
            blob: ObjectId::from([blob; 20]),
            page: false,
        }
    }

    #[test]
    fn a_file_is_renamed_only_when_the_same_blob_left_one_path_and_came_to_another() {
        let moved = moves(
            vec![file("b.zip", 1), file("a.zip", 1), file("gone.png", 2)],
            vec![file("y.zip", 1), file("new.png", 3), file("x.zip", 1)],
        );
        let renamed = |from: &str, path: &str| Moved::Renamed {
            from: from.to_owned(),
            path: path.to_owned(),
        };
        assert_eq!(
            moved,
            [
                Moved::Added(file("new.png", 3)),
                renamed("a.zip", "x.zip"),
                renamed("b.zip", "y.zip"),
                Moved::Deleted("gone.png".to_owned()),
            ]
        );
    }
}
