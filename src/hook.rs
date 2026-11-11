//! `mneme hook pre-receive`, the push check. Git runs it with the pushed objects in a
//! quarantine that plain `git` commands see, before any ref moves.

use std::fs;
use std::io::{self, BufRead};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, ExitCode};

use anyhow::{Context, Result, bail};

use crate::repo::{MAIN, is_page};

/// The largest page a push may hold.
const MAX_PAGE: u64 = 1 << 20;

/// Why a push is refused. Each reason is one line, which the pushing client shows.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("only {MAIN} can be pushed, not {0}")]
    OtherRef(String),
    #[error("{MAIN} cannot be deleted")]
    Deletion,
    #[error("commit {0} is a merge, and {MAIN} keeps one line of history")]
    Merge(String),
    #[error("commit {commit} has a symlink at {path}")]
    Symlink { commit: String, path: String },
    #[error("commit {commit} has a submodule at {path}")]
    Submodule { commit: String, path: String },
    #[error("commit {commit} has a path named .git: {path}")]
    DotGit { commit: String, path: String },
    #[error("commit {commit} has a page over 1 MiB: {path}")]
    LargePage { commit: String, path: String },
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
        for commit in git(&["rev-list", "-z", "--parents", new, "--not", "--all"])?
            .split(|&b| b == 0)
            .filter(|record| !record.is_empty())
        {
            let commit = String::from_utf8_lossy(commit);
            let mut ids = commit.split(' ');
            let id = ids.next().unwrap_or_default().to_owned();
            if ids.count() > 1 {
                return Ok(Err(Refusal::Merge(id)));
            }
            if let Err(refusal) = check_tree(&id)? {
                return Ok(Err(refusal));
            }
        }
    }
    Ok(Ok(()))
}

fn check_tree(commit: &str) -> Result<Result<(), Refusal>> {
    let format = "--format=%(objectmode) %(objecttype) %(objectsize)%x09%(path)";
    let listing = git(&["ls-tree", "-r", "-t", "-z", format, commit])?;
    for entry in listing.split(|&b| b == 0).filter(|entry| !entry.is_empty()) {
        let entry = String::from_utf8_lossy(entry);
        let Some((facts, path)) = entry.split_once('\t') else {
            bail!("unexpected ls-tree entry {entry:?}");
        };
        let mut facts = facts.split(' ');
        let (mode, kind, size) = (facts.next(), facts.next(), facts.next());
        let found =
            |make: fn(String, String) -> Refusal| Ok(Err(make(commit.to_owned(), path.to_owned())));
        if path
            .split('/')
            .any(|part| part.eq_ignore_ascii_case(".git"))
        {
            return found(|commit, path| Refusal::DotGit { commit, path });
        }
        if mode == Some("120000") {
            return found(|commit, path| Refusal::Symlink { commit, path });
        }
        if kind == Some("commit") {
            return found(|commit, path| Refusal::Submodule { commit, path });
        }
        let size: u64 = size.and_then(|size| size.parse().ok()).unwrap_or(0);
        if kind == Some("blob") && is_page(path.as_bytes()) && size > MAX_PAGE {
            return found(|commit, path| Refusal::LargePage { commit, path });
        }
    }
    Ok(Ok(()))
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
