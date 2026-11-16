//! `mneme hook pre-receive`, the push check. Git runs it with the pushed objects in a
//! quarantine that plain `git` commands see, before any ref moves.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead, Write as _};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, ExitCode, Stdio};

use anyhow::{Context, Result, bail};

use crate::repo::MAIN;
use crate::rules::{self, Broken, Entry};

/// Why a push is refused. Each reason is one line, which the pushing client shows.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("only {MAIN} can be pushed, not {0}")]
    OtherRef(String),
    #[error("{MAIN} cannot be deleted")]
    Deletion,
    #[error("commit {0} is a merge, and {MAIN} keeps one line of history")]
    Merge(String),
    #[error("commit {commit} has {broken} at {path}")]
    Broken {
        commit: String,
        path: String,
        broken: Broken,
    },
    /// The server reads a file over [`rules::MAX_WHOLE`] only as a stream, and a delta can only
    /// be rebuilt in memory.
    #[error(
        "{0} is over 10 MiB and the push stores it as a delta of another file, which the server \
         would have to hold in memory to read; set core.bigFileThreshold to 1m in this clone, \
         run git repack -a -d -F there, and push again"
    )]
    Delta(String),
}

/// Installs the hook into the bare repository at `repo`, as a script that runs this binary.
///
/// # Errors
///
/// Fails when the script cannot be written.
pub fn install(repo: &Path) -> Result<()> {
    let exe = std::env::current_exe().context("finding the mneme binary")?;
    let exe = exe
        .to_str()
        .context("the mneme binary's path is not UTF-8")?;
    let quoted = format!("'{}'", exe.replace('\'', r"'\''"));
    let path = repo.join("hooks").join("pre-receive");
    fs::create_dir_all(repo.join("hooks"))?;
    fs::write(
        &path,
        format!("#!/bin/sh\nexec {quoted} hook pre-receive\n"),
    )?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// Reads the ref updates on standard input and refuses the push when one breaks a rule.
pub fn pre_receive() -> ExitCode {
    let refusal = match check(io::stdin().lock()) {
        Ok(Ok(())) => return ExitCode::SUCCESS,
        Ok(Err(refusal)) => refusal.to_string(),
        Err(error) => format!("the push could not be checked: {error:#}"),
    };
    eprintln!("mneme: {refusal}");
    ExitCode::FAILURE
}

/// Checks each `old new ref` line of `updates`.
///
/// # Errors
///
/// Fails when a `git` command fails. The push is then refused too.
pub fn check(updates: impl BufRead) -> Result<Result<(), Refusal>> {
    for line in updates.lines() {
        let line = line?;
        let mut fields = line.split(' ');
        let (Some(_old), Some(new), Some(name)) = (fields.next(), fields.next(), fields.next())
        else {
            bail!("unexpected ref update {line:?}");
        };
        if name != MAIN {
            return Ok(Err(Refusal::OtherRef(name.to_owned())));
        }
        if new.bytes().all(|b| b == b'0') {
            return Ok(Err(Refusal::Deletion));
        }
        let listing = git(&["rev-list", "--parents", new, "--not", "--all"], b"")?;
        let mut large = BTreeMap::new();
        for (id, parents) in commits(&listing) {
            if parents > 1 {
                return Ok(Err(Refusal::Merge(id)));
            }
            if let Err(refusal) = check_tree(&id, &mut large)? {
                return Ok(Err(refusal));
            }
        }
        if let Some(path) = stored_as_delta(new, &large)? {
            return Ok(Err(Refusal::Delta(path)));
        }
    }
    Ok(Ok(()))
}

/// Each commit of a `git rev-list --parents` listing, with how many parents it has. It reads
/// one commit per line, since `rev-list` ends its records with NUL under `-z` only since Git 2.50.
fn commits(listing: &[u8]) -> Vec<(String, usize)> {
    String::from_utf8_lossy(listing)
        .lines()
        .map(|line| {
            let mut ids = line.split(' ');
            let id = ids.next().unwrap_or_default().to_owned();
            (id, ids.count())
        })
        .collect()
}

/// Checks every path of a commit against the rules, and adds each file over
/// [`rules::MAX_WHOLE`] to `large`, by its blob.
fn check_tree(commit: &str, large: &mut BTreeMap<String, String>) -> Result<Result<(), Refusal>> {
    let listing = git(&["ls-tree", "-r", "-t", "-z", "-l", commit], b"")?;
    for Listed { path, entry, blob } in entries(&listing)? {
        if let Some(broken) = rules::check(path.as_bytes(), entry) {
            return Ok(Err(Refusal::Broken {
                commit: commit.to_owned(),
                path,
                broken,
            }));
        }
        if matches!(entry, Entry::File(size) if size > rules::MAX_WHOLE) {
            large.insert(blob, path);
        }
    }
    Ok(Ok(()))
}

/// The path of a file among `large`, by blob, that this push brings and stores as a delta. The
/// server tells its own Git never to store a blob over a page's size as one, and the autosave
/// service tells its Git the same, but a push made by hand with Git's own settings can hold a
/// second version of a large file as a delta of the first. Git keeps a pack of a hundred objects
/// or more as it came, and such a blob could then only be read back by rebuilding it in memory.
fn stored_as_delta(new: &str, large: &BTreeMap<String, String>) -> Result<Option<String>> {
    if large.is_empty() {
        return Ok(None);
    }
    let pushed = git(&["rev-list", "--objects", new, "--not", "--all"], b"")?;
    let pushed = String::from_utf8_lossy(&pushed);
    let mut blobs = String::new();
    for id in pushed.lines().filter_map(|line| line.split(' ').next()) {
        if large.contains_key(id) {
            blobs.push_str(id);
            blobs.push('\n');
        }
    }
    if blobs.is_empty() {
        return Ok(None);
    }
    let stored = git(
        &["cat-file", "--batch-check=%(objectname) %(deltabase)"],
        blobs.as_bytes(),
    )?;
    Ok(String::from_utf8_lossy(&stored).lines().find_map(|line| {
        let (blob, base) = line.split_once(' ')?;
        base.bytes()
            .any(|b| b != b'0')
            .then(|| large.get(blob).cloned())
            .flatten()
    }))
}

/// One path of a tree listing.
#[derive(Debug, PartialEq, Eq)]
struct Listed {
    path: String,
    entry: Entry,
    /// The object the path holds, as Git names it.
    blob: String,
}

/// Each path of a `git ls-tree -r -t -z -l` listing, with what it holds. The long listing gives
/// each path as it is, where `--format`'s `%(path)` quotes one with a tab, a newline or a byte
/// over 0x7f even under `-z`, so that `café.md` would not end in `.md`.
fn entries(listing: &[u8]) -> Result<Vec<Listed>> {
    listing
        .split(|&b| b == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let entry = String::from_utf8_lossy(entry);
            let Some((facts, path)) = entry.split_once('\t') else {
                bail!("unexpected ls-tree entry {entry:?}");
            };
            let (held, blob) = match facts.split_whitespace().collect::<Vec<_>>()[..] {
                ["120000", "blob", blob, _] => (Entry::Symlink, blob),
                [_, "blob", blob, size] => (Entry::File(size.parse()?), blob),
                [_, "tree", blob, _] => (Entry::Folder, blob),
                [_, "commit", blob, _] => (Entry::Repository, blob),
                _ => bail!("unexpected ls-tree entry {entry:?}"),
            };
            Ok(Listed {
                path: path.to_owned(),
                entry: held,
                blob: blob.to_owned(),
            })
        })
        .collect()
}

/// Runs `git` with `input` on standard input and returns what it printed.
fn git(args: &[&str], input: &[u8]) -> Result<Vec<u8>> {
    let mut child = Command::new("git")
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("running git")?;
    let mut stdin = child.stdin.take().context("no stdin")?;
    // Git answers as it reads, so the input is written while its output is read.
    let output = std::thread::scope(|scope| {
        scope.spawn(move || stdin.write_all(input));
        child.wait_with_output()
    })
    .context("running git")?;
    if !output.status.success() {
        bail!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What each Git prints for one repository, saved by `scripts/git-output.sh`.
    const GITS: [(&str, &[u8], &[u8]); 2] = [
        (
            "2.49.1",
            include_bytes!("../tests/fixtures/git/2.49.1/rev-list"),
            include_bytes!("../tests/fixtures/git/2.49.1/ls-tree"),
        ),
        (
            "2.56.0",
            include_bytes!("../tests/fixtures/git/2.56.0/rev-list"),
            include_bytes!("../tests/fixtures/git/2.56.0/ls-tree"),
        ),
    ];

    #[test]
    fn each_commit_is_read_with_its_parents_from_both_gits() {
        for (version, rev_list, _) in GITS {
            let commits = commits(rev_list);
            let parents: Vec<usize> = commits.iter().map(|(_, parents)| *parents).collect();
            assert_eq!(
                parents,
                [1, 2, 1, 1, 0],
                "Git {version}: one merge, one root"
            );
            assert!(
                commits
                    .iter()
                    .all(|(id, _)| id.len() == 40 && id.bytes().all(|b| b.is_ascii_hexdigit())),
                "Git {version}: every id is a whole commit id and nothing more: {commits:?}"
            );
        }
    }

    #[test]
    fn each_entry_is_read_with_its_path_as_it_is_from_both_gits() {
        let expected = [
            ("a.md", Entry::File(4)),
            ("c.md", Entry::File(4)),
            ("link.md", Entry::Symlink),
            ("new\nline.md", Entry::File(8)),
            ("notes", Entry::Folder),
            ("notes/b.md", Entry::File(4)),
            ("run.sh", Entry::File(10)),
            ("side.md", Entry::File(7)),
            ("tab\tname.md", Entry::File(4)),
            ("vendor", Entry::Folder),
            ("vendor/lib", Entry::Repository),
            ("with space.md", Entry::File(6)),
            ("ö.md", Entry::File(7)),
        ]
        .map(|(path, entry)| (path.to_owned(), entry));
        for (version, _, ls_tree) in GITS {
            let listed = entries(ls_tree).unwrap();
            assert!(
                listed.iter().all(|listed| listed.blob.len() == 40),
                "Git {version}: every entry names its object: {listed:?}"
            );
            let read: Vec<_> = listed
                .into_iter()
                .map(|listed| (listed.path, listed.entry))
                .collect();
            assert_eq!(read, expected, "Git {version}");
        }
    }
}
