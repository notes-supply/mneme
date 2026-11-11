//! A save: stage each change that may be saved, check what was staged against the rules, and
//! commit it as the machine.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;

use anyhow::{Result, bail};

use super::folder::{self, Change, Folder, Leave};
use super::git::{self, Git, NO_OBJECT};
use crate::rules::{self, Entry};

pub(super) enum Save {
    Saved(String),
    Stopped(String),
    Nothing,
}

/// A path the service leaves in the folder, and why.
#[derive(Debug)]
pub(super) struct Left {
    pub path: Vec<u8>,
    pub leave: Leave,
}

/// The paths the folder's look leaves out.
pub(super) fn left(folder: &Folder) -> Vec<Left> {
    folder
        .changes
        .iter()
        .filter_map(|changed| {
            changed.leave.map(|leave| Left {
                path: changed.path.clone(),
                leave,
            })
        })
        .collect()
}

/// Commits every change that may be saved, unless that would delete more than half of the
/// pages and more than five. Returns what the staging found it had to leave out as well.
pub(super) fn save(git: &Git, machine: &str, folder: &Folder) -> Result<(Save, Vec<Left>)> {
    let lost = folder.pages_lost();
    if lost > 5 {
        let tracked = match &folder.head {
            Some(head) => git
                .ok(&["ls-tree", "-r", "-z", "--name-only", head])?
                .split(|&b| b == 0)
                .filter(|path| folder::is_page(path))
                .count(),
            None => 0,
        };
        if lost * 2 > tracked {
            let reason = format!("the save would delete {lost} of {tracked} pages");
            return Ok((Save::Stopped(reason), Vec::new()));
        }
    }
    let mut paths = Vec::new();
    for changed in folder.saveable() {
        paths.extend_from_slice(&changed.path);
        paths.push(0);
    }
    git.ok_with(
        &["update-index", "--add", "--remove", "-z", "--stdin"],
        &[],
        &paths,
    )?;
    let (left, staged) = unstage_what_breaks_a_rule(git, folder)?;
    if staged == 0 {
        return Ok((Save::Nothing, left));
    }
    let message = format!("Autosave from {machine}");
    let email = git::email(machine);
    let author = [
        ("GIT_AUTHOR_NAME", machine),
        ("GIT_AUTHOR_EMAIL", email.as_str()),
        ("GIT_AUTHOR_DATE", git.date()),
    ];
    git.ok_with(
        &["commit", "--quiet", "--no-verify", "-m", &message],
        &author,
        &[],
    )?;
    Ok((Save::Saved(git.line(&["rev-parse", "HEAD"])?), left))
}

/// One entry of `git diff --cached --raw`.
struct Staged<'a> {
    old_mode: &'a str,
    old_id: &'a str,
    new_mode: &'a str,
    new_id: &'a str,
    deleted: bool,
    path: &'a [u8],
}

/// Checks what is staged, not what the folder looked like: a file can change between the look
/// and the staging, and a person can stage one by hand. Each entry that breaks a rule goes back
/// to what `HEAD` has. A new file that is still in the folder and was not staged is one Git
/// refuses to store. Returns those, and how many changes stay staged.
fn unstage_what_breaks_a_rule(git: &Git, folder: &Folder) -> Result<(Vec<Left>, usize)> {
    let raw = git.ok(&[
        "diff",
        "--cached",
        "--raw",
        "-z",
        "--no-renames",
        "--no-abbrev",
    ])?;
    let staged = parse_raw(&raw)?;
    let blobs: Vec<&str> = staged
        .iter()
        .filter(|entry| !entry.deleted && matches!(entry.new_mode, "100644" | "100755"))
        .map(|entry| entry.new_id)
        .collect();
    let sizes = if blobs.is_empty() {
        Vec::new()
    } else {
        let input = blobs.join("\n") + "\n";
        let output = git.ok_with(
            &["cat-file", "--batch-check=%(objectsize)"],
            &[],
            input.as_bytes(),
        )?;
        String::from_utf8_lossy(&output)
            .lines()
            .map(str::parse)
            .collect::<Result<Vec<u64>, _>>()?
    };
    let mut sizes = sizes.into_iter();
    let (mut left, mut restore) = (Vec::new(), Vec::new());
    for entry in staged.iter().filter(|entry| !entry.deleted) {
        let kind = match entry.new_mode {
            "120000" => Entry::Symlink,
            "160000" => Entry::Repository,
            _ => Entry::File(sizes.next().unwrap_or_default()),
        };
        let Some(broken) = rules::check(entry.path, kind) else {
            continue;
        };
        let (mode, id) = if entry.old_mode == "000000" {
            ("0", NO_OBJECT)
        } else {
            (entry.old_mode, entry.old_id)
        };
        restore.extend_from_slice(format!("{mode} {id}\t").as_bytes());
        restore.extend_from_slice(entry.path);
        restore.push(0);
        left.push(Left {
            path: entry.path.to_vec(),
            leave: Leave::Broken(broken),
        });
    }
    let staying = staged.len() - left.len();
    if !restore.is_empty() {
        git.ok_with(&["update-index", "-z", "--index-info"], &[], &restore)?;
    }
    let paths: HashSet<&[u8]> = staged.iter().map(|entry| entry.path).collect();
    for changed in folder.saveable() {
        if changed.change == Change::Added
            && !paths.contains(&changed.path[..])
            && folder_has(git, &changed.path)
        {
            left.push(Left {
                path: changed.path.clone(),
                leave: Leave::Unstorable,
            });
        }
    }
    Ok((left, staying))
}

fn folder_has(git: &Git, path: &[u8]) -> bool {
    git.folder()
        .join(OsStr::from_bytes(path))
        .symlink_metadata()
        .is_ok()
}

/// Reads `:old_mode new_mode old_id new_id status NUL path NUL` records.
fn parse_raw(raw: &[u8]) -> Result<Vec<Staged<'_>>> {
    let mut fields = raw.split(|&b| b == 0);
    let mut staged = Vec::new();
    while let Some(facts) = fields.next().filter(|facts| !facts.is_empty()) {
        let Some(path) = fields.next() else {
            bail!("a diff entry without a path");
        };
        let facts = std::str::from_utf8(facts)?;
        let parts: Vec<&str> = facts.trim_start_matches(':').split(' ').collect();
        let [old_mode, new_mode, old_id, new_id, status] = parts[..] else {
            bail!("unexpected diff entry {facts:?}");
        };
        staged.push(Staged {
            old_mode,
            old_id,
            new_mode,
            new_id,
            deleted: status == "D",
            path,
        });
    }
    Ok(staged)
}
