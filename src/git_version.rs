//! The oldest Git that `mneme serve` and `mneme autosave` work with, and the check that refuses
//! an older one before anything is changed.

use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};

/// The service's replay is the newest need. `git merge-tree --write-tree` came in 2.38 and its
/// `--merge-base` in 2.40, and since 2.45 its inputs may be trees, which the replay needs for a
/// commit with no parent and for a server with no `main` yet. The server's newest need is
/// `git ls-tree --format` in the push check, since 2.36.
pub const OLDEST: (u32, u32, u32) = (2, 45, 0);

/// Refuses a Git older than [`OLDEST`], saying which version `program` is and that `who`
/// needs a newer one.
///
/// # Errors
///
/// Fails when `program` cannot be run, its version cannot be read, or it is too old.
pub fn check(program: &Path, who: &str) -> Result<()> {
    let output = Command::new(program)
        .arg("--version")
        .output()
        .with_context(|| format!("running {} --version", program.display()))?;
    let said = String::from_utf8_lossy(&output.stdout);
    let said = said.trim();
    let Some(version) = parse(said) else {
        bail!("cannot read the Git version from {said:?}");
    };
    if version < OLDEST {
        let (major, minor, patch) = OLDEST;
        bail!("{said} is too old: {who} needs Git {major}.{minor}.{patch} or newer");
    }
    Ok(())
}

/// The version in `git version 2.45.2`, `git version 2.39.3 (Apple Git-145)` or
/// `git version 2.45.2.windows.1`.
fn parse(said: &str) -> Option<(u32, u32, u32)> {
    let number = said
        .strip_prefix("git version ")?
        .split_whitespace()
        .next()?;
    let mut parts = number.split('.').map(|part| {
        let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
        digits.parse().ok()
    });
    let major = parts.next()??;
    let minor = parts.next()??;
    let patch = parts.next().flatten().unwrap_or(0);
    Some((major, minor, patch))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_git_version_is_read_from_each_way_git_prints_it() {
        for (said, version) in [
            ("git version 2.56.0", Some((2, 56, 0))),
            ("git version 2.39.3 (Apple Git-145)", Some((2, 39, 3))),
            ("git version 2.45.2.windows.1", Some((2, 45, 2))),
            ("git version 2.45.0-rc0", Some((2, 45, 0))),
            ("git version 3.0", Some((3, 0, 0))),
            ("hub version 2.14", None),
            ("git version unknown", None),
        ] {
            assert_eq!(parse(said), version, "{said}");
        }
    }
}
