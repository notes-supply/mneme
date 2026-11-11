//! `mneme hook pre-receive`, the push check. Git runs it with the pushed objects in a
//! quarantine that plain `git` commands see, before any ref moves.

use std::fs;
use std::io::{self, BufRead};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, ExitCode};

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
        let listing = git(&["rev-list", "--parents", new, "--not", "--all"])?;
        for (id, parents) in commits(&listing) {
            if parents > 1 {
                return Ok(Err(Refusal::Merge(id)));
            }
            if let Err(refusal) = check_tree(&id)? {
                return Ok(Err(refusal));
            }
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

fn check_tree(commit: &str) -> Result<Result<(), Refusal>> {
    let listing = git(&["ls-tree", "-r", "-t", "-z", "-l", commit])?;
    for (path, entry) in entries(&listing)? {
        if let Some(broken) = rules::check(path.as_bytes(), entry) {
            return Ok(Err(Refusal::Broken {
                commit: commit.to_owned(),
                path,
                broken,
            }));
        }
    }
    Ok(Ok(()))
}

/// Each path of a `git ls-tree -r -t -z -l` listing, with what it holds. The long listing gives
/// each path as it is, where `--format`'s `%(path)` quotes one with a tab, a newline or a byte
/// over 0x7f even under `-z`, so that `café.md` would not end in `.md`.
fn entries(listing: &[u8]) -> Result<Vec<(String, Entry)>> {
    listing
        .split(|&b| b == 0)
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let entry = String::from_utf8_lossy(entry);
            let Some((facts, path)) = entry.split_once('\t') else {
                bail!("unexpected ls-tree entry {entry:?}");
            };
            let held = match facts.split_whitespace().collect::<Vec<_>>()[..] {
                ["120000", "blob", _, _] => Entry::Symlink,
                [_, "blob", _, size] => Entry::File(size.parse()?),
                [_, "tree", _, _] => Entry::Folder,
                [_, "commit", _, _] => Entry::Repository,
                _ => bail!("unexpected ls-tree entry {entry:?}"),
            };
            Ok((path.to_owned(), held))
        })
        .collect()
}

fn git(args: &[&str]) -> Result<Vec<u8>> {
    let output = Command::new("git")
        .args(args)
        .output()
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
            assert_eq!(entries(ls_tree).unwrap(), expected, "Git {version}");
        }
    }
}
