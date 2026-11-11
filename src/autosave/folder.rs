//! What the folder looks like at the start of a tick, from one `git status`.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::time::{Duration, SystemTime};

use anyhow::{Result, bail};

use super::git::Git;

/// How a path differs from `HEAD` once everything is staged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Change {
    Added,
    Modified,
    Deleted,
}

#[derive(Debug)]
pub(super) struct Folder {
    /// `None` while `main` has no commit.
    pub head: Option<String>,
    /// `None` when `HEAD` is detached.
    pub branch: Option<String>,
    pub changes: Vec<(Vec<u8>, Change)>,
}

impl Folder {
    pub fn read(git: &Git) -> Result<Self> {
        let stdout = git.ok(&[
            "status",
            "--porcelain=v2",
            "-z",
            "--branch",
            "--untracked-files=all",
            "--no-renames",
            "--ignore-submodules=all",
        ])?;
        parse(&stdout)
    }

    pub fn clean(&self) -> bool {
        self.changes.is_empty()
    }

    /// Whether every changed file that still exists was last written before `now - quiet`.
    pub fn quiet(&self, folder: &Path, now: SystemTime, quiet: Duration) -> bool {
        self.changes.iter().all(|(path, _)| {
            match folder.join(OsStr::from_bytes(path)).symlink_metadata() {
                Ok(metadata) => metadata
                    .modified()
                    .is_ok_and(|modified| modified + quiet <= now),
                Err(_) => true,
            }
        })
    }

    /// Pages a save would remove, less the pages it would add, so that a page moved to a new
    /// path is not counted as deleted.
    pub fn pages_lost(&self) -> usize {
        let count = |wanted: Change| {
            self.changes
                .iter()
                .filter(|(path, change)| *change == wanted && is_page(path))
                .count()
        };
        count(Change::Deleted).saturating_sub(count(Change::Added))
    }
}

pub(super) fn is_page(path: &[u8]) -> bool {
    path.ends_with(b".md")
}

fn parse(stdout: &[u8]) -> Result<Folder> {
    let mut folder = Folder {
        head: None,
        branch: None,
        changes: Vec::new(),
    };
    for record in stdout
        .split(|&b| b == 0)
        .filter(|record| !record.is_empty())
    {
        if let Some(header) = record.strip_prefix(b"# ") {
            let text = String::from_utf8_lossy(header);
            if let Some(oid) = text.strip_prefix("branch.oid ") {
                folder.head = (oid != "(initial)").then(|| oid.to_owned());
            } else if let Some(name) = text.strip_prefix("branch.head ") {
                folder.branch = (name != "(detached)").then(|| name.to_owned());
            }
            continue;
        }
        let fields = match record.first() {
            Some(b'?') => {
                folder.changes.push((record[2..].to_vec(), Change::Added));
                continue;
            }
            Some(b'!') => continue,
            Some(b'1') => 9,
            Some(b'u') => 11,
            _ => bail!(
                "unexpected status record {:?}",
                String::from_utf8_lossy(record)
            ),
        };
        let parts: Vec<&[u8]> = record.splitn(fields, |&b| b == b' ').collect();
        let (Some(xy), Some(path)) = (parts.get(1), parts.get(fields - 1)) else {
            bail!(
                "a status record without a path: {:?}",
                String::from_utf8_lossy(record)
            );
        };
        let change = match xy {
            [b'A', b'D'] => continue,
            [b'A', _] => Change::Added,
            [b'D', _] | [_, b'D'] => Change::Deleted,
            _ => Change::Modified,
        };
        folder.changes.push((path.to_vec(), change));
    }
    Ok(folder)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_status_lists_the_head_branch_and_every_change() {
        let status = b"# branch.oid 1111111111111111111111111111111111111111\0\
            # branch.head main\0\
            1 .M N... 100644 100644 100644 aaaa aaaa notes/a page.md\0\
            1 D. N... 100644 000000 000000 bbbb 0000 gone.md\0\
            1 .D N... 100644 100644 000000 cccc cccc also gone.md\0\
            1 A. N... 000000 100644 100644 0000 dddd added.md\0\
            1 AD N... 000000 100644 000000 0000 eeee added then removed.md\0\
            ? new page.md\0";
        let folder = parse(status).unwrap();
        assert_eq!(
            folder.head.as_deref(),
            Some("1111111111111111111111111111111111111111")
        );
        assert_eq!(folder.branch.as_deref(), Some("main"));
        let changes: Vec<(&str, Change)> = folder
            .changes
            .iter()
            .map(|(path, change)| (std::str::from_utf8(path).unwrap(), *change))
            .collect();
        assert_eq!(
            changes,
            [
                ("notes/a page.md", Change::Modified),
                ("gone.md", Change::Deleted),
                ("also gone.md", Change::Deleted),
                ("added.md", Change::Added),
                ("new page.md", Change::Added),
            ]
        );
        assert_eq!(folder.pages_lost(), 0);
    }

    #[test]
    fn an_unborn_or_detached_head_has_no_commit_or_no_branch() {
        let unborn = parse(b"# branch.oid (initial)\0# branch.head main\0").unwrap();
        assert_eq!(
            (unborn.head, unborn.branch.as_deref()),
            (None, Some("main"))
        );
        let detached = parse(
            b"# branch.oid 1111111111111111111111111111111111111111\0# branch.head (detached)\0",
        )
        .unwrap();
        assert_eq!(detached.branch, None);
    }
}
