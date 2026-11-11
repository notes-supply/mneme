//! What the wiki may hold. The push check refuses a commit that breaks one of these rules, and
//! the autosave service leaves a file that breaks one in the folder rather than commit it, since
//! a commit the server refuses would hold back every save after it.

use crate::repo::is_page;

/// The largest page.
pub const MAX_PAGE: u64 = 1 << 20;
/// The largest file of any kind.
pub const MAX_FILE: u64 = 10 << 20;

/// What a path holds, as Git would store it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry {
    Folder,
    /// A file of this many bytes.
    File(u64),
    Symlink,
    /// A submodule, or a repository inside the folder, which Git would store as one.
    Repository,
}

/// A rule that a path breaks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Broken {
    #[error("a path named .git")]
    DotGit,
    #[error("a symlink")]
    Symlink,
    #[error("a submodule or nested repository")]
    Repository,
    #[error("a page over 1 MiB")]
    LargePage,
    #[error("a file over 10 MiB")]
    LargeFile,
}

/// The rule that `path`, holding `entry`, breaks, if any. A path component is `.git` in any
/// case, since clones on macOS and Windows read `.GIT` as `.git`.
#[must_use]
pub fn check(path: &[u8], entry: Entry) -> Option<Broken> {
    if path
        .split(|&b| b == b'/')
        .any(|part| part.eq_ignore_ascii_case(b".git"))
    {
        return Some(Broken::DotGit);
    }
    match entry {
        Entry::Symlink => Some(Broken::Symlink),
        Entry::Repository => Some(Broken::Repository),
        Entry::File(size) if is_page(path) && size > MAX_PAGE => Some(Broken::LargePage),
        Entry::File(size) if size > MAX_FILE => Some(Broken::LargeFile),
        Entry::Folder | Entry::File(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_rule_and_its_limit() {
        let cases: [(&[u8], Entry, Option<Broken>); 11] = [
            (b"notes/a.md", Entry::File(MAX_PAGE), None),
            (
                b"notes/a.md",
                Entry::File(MAX_PAGE + 1),
                Some(Broken::LargePage),
            ),
            (b"a.bin", Entry::File(MAX_FILE), None),
            (b"a.bin", Entry::File(MAX_FILE + 1), Some(Broken::LargeFile)),
            (b"a.md", Entry::File(MAX_FILE + 1), Some(Broken::LargePage)),
            (b"link.md", Entry::Symlink, Some(Broken::Symlink)),
            (b"vendor/lib", Entry::Repository, Some(Broken::Repository)),
            (b"x/.GIT/config", Entry::File(1), Some(Broken::DotGit)),
            (b".git", Entry::Folder, Some(Broken::DotGit)),
            (b"a.git/b.gitx", Entry::File(1), None),
            (b"notes", Entry::Folder, None),
        ];
        for (path, entry, broken) in cases {
            assert_eq!(
                check(path, entry),
                broken,
                "{}",
                String::from_utf8_lossy(path)
            );
        }
    }
}
