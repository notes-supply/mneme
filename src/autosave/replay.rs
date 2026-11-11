//! Replays local commits onto the server's `main` with Git's own three-way merge, writing only
//! objects. Nothing in the folder changes until the caller moves `main` to the result, so a
//! kill at any point leaves no rebase behind and the next tick starts over.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

use anyhow::{Context, Result, bail};

use super::git::Git;

/// The tree of a commit with no parent is compared with the empty tree.
pub(super) const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// For each path that did not merge, the replayed commit's mode and blob, or `None` when that
/// commit has no file there.
type Sides = BTreeMap<Vec<u8>, Option<(String, String)>>;

/// A local commit to replay, with the parent its change is measured from.
pub(super) struct Local {
    pub id: String,
    pub parent: Option<String>,
}

/// Where the replay ends: the new tip, or `None` when every commit came out empty and there
/// was no base.
pub(super) struct Replayed {
    pub tip: Option<String>,
}

/// Replays `commits`, oldest first, onto `onto`, or as a new line of history when `onto` is
/// `None`. A commit whose change is already there is left out.
pub(super) fn replay(git: &Git, onto: Option<&str>, commits: &[Local]) -> Result<Replayed> {
    let mut tip = onto.map(str::to_owned);
    let mut tip_tree = match &tip {
        Some(commit) => git.line(&["rev-parse", &format!("{commit}^{{tree}}")])?,
        None => EMPTY_TREE.to_owned(),
    };
    for commit in commits {
        let base = commit.parent.as_deref().unwrap_or(EMPTY_TREE);
        let ours = tip.as_deref().unwrap_or(EMPTY_TREE);
        let merge_base = format!("--merge-base={base}");
        let run = git.run(&[
            "merge-tree",
            "--write-tree",
            "-z",
            "--no-messages",
            &merge_base,
            ours,
            &commit.id,
        ])?;
        let mut records = run.stdout.split(|&b| b == 0);
        let merged = String::from_utf8_lossy(records.next().unwrap_or_default()).into_owned();
        let (tree, overlap) = if run.success {
            (merged, None)
        } else if run.code == Some(1) {
            let paths = theirs_per_path(records.take_while(|record| !record.is_empty()))?;
            let tree = resolve(git, &merged, &paths)?;
            let overlap = match &tip {
                Some(tip) => Some(overlapped(git, tip, commit.parent.as_deref(), &paths)?),
                None => None,
            };
            (tree, overlap)
        } else {
            bail!("git merge-tree failed: {}", run.error());
        };
        if tree == tip_tree {
            continue;
        }
        tip = Some(rewrite(
            git,
            commit,
            &tree,
            tip.as_deref(),
            overlap.as_deref(),
        )?);
        tip_tree = tree;
    }
    Ok(Replayed { tip })
}

/// For each path that did not merge, the entry of the commit being replayed, or `None` when
/// that commit has no file there. That side wins: the local edit stays, and so does a local
/// delete.
fn theirs_per_path<'a>(records: impl Iterator<Item = &'a [u8]>) -> Result<Sides> {
    let mut paths = BTreeMap::new();
    for record in records {
        let Some(tab) = record.iter().position(|&b| b == b'\t') else {
            bail!(
                "unexpected merge-tree entry {:?}",
                String::from_utf8_lossy(record)
            );
        };
        let facts = String::from_utf8_lossy(&record[..tab]).into_owned();
        let facts: Vec<&str> = facts.split(' ').collect();
        let entry = paths.entry(record[tab + 1..].to_vec()).or_insert(None);
        if let [mode, oid, "3"] = facts[..] {
            *entry = Some((mode.to_owned(), oid.to_owned()));
        }
    }
    Ok(paths)
}

/// The merged tree with each unmerged path set to the replayed commit's side, built in an index
/// of its own so the folder's index is never touched.
fn resolve(git: &Git, merged: &str, paths: &Sides) -> Result<String> {
    let index = git.git_dir().join("mneme-replay-index");
    let index_text = index.to_string_lossy().into_owned();
    let with_index = [("GIT_INDEX_FILE", index_text.as_str())];
    git.ok_with(&["read-tree", merged], &with_index, &[])?;
    let mut entries = Vec::new();
    for (path, side) in paths {
        match side {
            Some((mode, oid)) => entries.extend_from_slice(format!("{mode} {oid}\t").as_bytes()),
            None => entries.extend_from_slice(format!("0 {}\t", "0".repeat(40)).as_bytes()),
        }
        entries.extend_from_slice(path);
        entries.push(0);
    }
    git.ok_with(
        &["update-index", "-z", "--index-info"],
        &with_index,
        &entries,
    )?;
    let tree = git.ok_with(&["write-tree"], &with_index, &[])?;
    let _ = std::fs::remove_file(&index);
    Ok(String::from_utf8_lossy(&tree).trim().to_owned())
}

/// The newest commit on the base's side that changed one of `paths`: the edit that lost.
fn overlapped(git: &Git, tip: &str, parent: Option<&str>, paths: &Sides) -> Result<String> {
    let mut command = vec!["rev-list", "-1", tip];
    let excluded = parent.map(|parent| format!("^{parent}"));
    if let Some(excluded) = &excluded {
        command.push(excluded);
    }
    command.push("--");
    let paths: Vec<&str> = paths
        .keys()
        .map(|path| OsStr::from_bytes(path).to_str().unwrap_or_default())
        .filter(|path| !path.is_empty())
        .collect();
    command.extend(paths);
    let found = git.line(&command)?;
    Ok(if found.is_empty() {
        tip.to_owned()
    } else {
        found
    })
}

/// A copy of `commit` with `tree` on `parent`. It keeps the author, the author's time and the
/// message, and gains an `Overlap:` trailer when a side had to be picked.
fn rewrite(
    git: &Git,
    commit: &Local,
    tree: &str,
    parent: Option<&str>,
    overlap: Option<&str>,
) -> Result<String> {
    let raw = git.ok(&["cat-file", "commit", &commit.id])?;
    let split = raw
        .windows(2)
        .position(|pair| pair == b"\n\n")
        .context("a commit without a message")?;
    let (headers, message) = (&raw[..split], &raw[split + 2..]);
    let author = String::from_utf8_lossy(headers)
        .lines()
        .find_map(|line| line.strip_prefix("author ").map(str::to_owned))
        .context("a commit without an author")?;
    let (ident, date) = author
        .rsplit_once("> ")
        .context("an author line without a date")?;
    let (name, email) = ident
        .rsplit_once(" <")
        .context("an author line without an email")?;
    let message = match overlap {
        Some(overlap) => git.ok_with(
            &[
                "interpret-trailers",
                "--trailer",
                &format!("Overlap: {overlap}"),
            ],
            &[],
            message,
        )?,
        None => message.to_vec(),
    };
    let author_date = format!("@{date}");
    let author = [
        ("GIT_AUTHOR_NAME", name),
        ("GIT_AUTHOR_EMAIL", email),
        ("GIT_AUTHOR_DATE", author_date.as_str()),
    ];
    let mut args = vec!["commit-tree", tree];
    if let Some(parent) = parent {
        args.extend(["-p", parent]);
    }
    let id = git.ok_with(&args, &author, &message)?;
    Ok(String::from_utf8_lossy(&id).trim().to_owned())
}
