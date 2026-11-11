//! Synthetic wiki histories made with the stock `git` command, isolated from the machine's
//! own Git configuration.

use std::cell::Cell;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

/// The author time of the first fixture commit: 2026-09-01T00:00:00Z.
pub const START: i64 = 1_788_220_800;

/// A Git program for the autosave service that runs `.git/mneme-test-race` once, right before the
/// first command whose words include those in `.git/mneme-test-race.before`, and then the command
/// itself. The script is renamed `mneme-test-race.ran` as it starts.
const RACY_GIT: &str = r#"#!/bin/sh
race="$GIT_DIR/mneme-test-race"
if [ -n "$GIT_DIR" ] && [ -f "$race" ]; then
  before=$(cat "$race.before")
  case " $* " in
    *" $before "*) mv "$race" "$race.ran" && sh "$race.ran" ;;
  esac
fi
exec git "$@"
"#;

/// Writes [`RACY_GIT`] into `dir` and returns its path.
pub fn racy_git(dir: &Path) -> PathBuf {
    let program = dir.join("racy-git");
    fs::write(&program, RACY_GIT).unwrap();
    fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
    program
}

/// Empty configuration for every `git` the tests run. The machine's global gitignore would
/// otherwise hide files such as `AGENTS.md`.
pub struct Isolation {
    home: TempDir,
}

impl Isolation {
    pub fn new() -> Self {
        let home = TempDir::new().unwrap();
        fs::create_dir(home.path().join("xdg")).unwrap();
        fs::write(home.path().join("gitconfig"), "").unwrap();
        Self { home }
    }

    pub fn apply<'a>(&self, command: &'a mut Command) -> &'a mut Command {
        let home = self.home.path();
        command
            .env("HOME", home)
            .env("XDG_CONFIG_HOME", home.join("xdg"))
            .env("GIT_CONFIG_GLOBAL", home.join("gitconfig"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_AUTHOR_NAME", "Writer")
            .env("GIT_AUTHOR_EMAIL", "writer@example.invalid")
            .env("GIT_COMMITTER_NAME", "Writer")
            .env("GIT_COMMITTER_EMAIL", "writer@example.invalid")
    }

    pub fn git(&self, dir: &Path, args: &[&str]) -> Output {
        self.apply(Command::new("git").current_dir(dir).args(args))
            .output()
            .unwrap()
    }

    /// Runs `git` and returns its standard output, failing the test when it fails.
    pub fn ok(&self, dir: &Path, args: &[&str]) -> String {
        let output = self.git(dir, args);
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }
}

/// A clone a writer edits.
pub struct Work {
    dir: TempDir,
    pub git: Isolation,
    clock: Cell<i64>,
}

impl Work {
    pub fn new() -> Self {
        let work = Self {
            dir: TempDir::new().unwrap(),
            git: Isolation::new(),
            clock: Cell::new(START),
        };
        work.ok(&["init", "-q", "-b", "main"]);
        work
    }

    /// A clone of `url` that sends `header` on every request.
    pub fn clone_from(url: &str, header: &str) -> Self {
        let work = Self {
            dir: TempDir::new().unwrap(),
            git: Isolation::new(),
            clock: Cell::new(START),
        };
        let extra = format!("http.extraHeader={header}");
        work.git
            .ok(work.path(), &["-c", &extra, "clone", "-q", url, "."]);
        work.ok(&["config", "http.extraHeader", header]);
        work
    }

    /// Pushes, and returns whether it worked and what Git said.
    pub fn push(&self, args: &[&str]) -> (bool, String) {
        let output = self.git.git(self.path(), &[&["push"], args].concat());
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }

    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    pub fn file(&self, path: &str) -> PathBuf {
        self.path().join(path)
    }

    pub fn ok(&self, args: &[&str]) -> String {
        self.git.ok(self.path(), args)
    }

    /// Runs `git` with `input` on standard input and returns its standard output.
    pub fn git_stdin(&self, args: &[&str], input: &str) -> String {
        use std::io::Write as _;
        let mut child = self
            .git
            .apply(Command::new("git").current_dir(self.path()).args(args))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "git {args:?} failed");
        String::from_utf8(output.stdout).unwrap()
    }

    pub fn write(&self, path: &str, text: &str) {
        let file = self.file(path);
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(file, text).unwrap();
    }

    pub fn remove(&self, path: &str) {
        fs::remove_file(self.file(path)).unwrap();
    }

    pub fn rename(&self, from: &str, to: &str) {
        let to = self.file(to);
        fs::create_dir_all(to.parent().unwrap()).unwrap();
        fs::rename(self.file(from), to).unwrap();
    }

    /// Commits every change in the folder, ten minutes after the last commit.
    pub fn commit(&self, message: &str) -> String {
        let time = self.clock.get() + 600;
        self.commit_at(message, time)
    }

    pub fn commit_at(&self, message: &str, time: i64) -> String {
        self.clock.set(time);
        self.ok(&["add", "-A"]);
        let date = format!("@{time} +0000");
        let output = self
            .git
            .apply(
                Command::new("git")
                    .current_dir(self.path())
                    .args(["commit", "-q", "-m", message]),
            )
            .env("GIT_AUTHOR_DATE", &date)
            .env("GIT_COMMITTER_DATE", &date)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "commit failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        self.ok(&["rev-parse", "HEAD"]).trim().to_owned()
    }

    /// Commits whose text of the page changed, by `git log --follow`, between the commit that
    /// created the page and `last`. A pure rename keeps the blob and is not counted, and nor
    /// is the deletion.
    pub fn text_changes(&self, path: &str, created: &str, last: &str) -> usize {
        let parent = format!("{created}^");
        let has_parent = self
            .git
            .git(self.path(), &["rev-parse", "--verify", "-q", &parent]);
        let range = if has_parent.status.success() {
            format!("{parent}..{last}")
        } else {
            last.to_owned()
        };
        self.ok(&[
            "log",
            "--follow",
            "--raw",
            "--no-abbrev",
            "--format=",
            &range,
            "--",
            path,
        ])
        .lines()
        .filter(|line| {
            let mut fields = line.trim_start_matches(':').split_whitespace();
            let (old, new, status) = (fields.nth(2), fields.next(), fields.next());
            old != new && status != Some("D")
        })
        .count()
    }
}

/// The history the tests read: every kind of change a writer makes.
pub fn standard() -> Work {
    let work = Work::new();
    work.write("index.md", "# Home\n\nThe wiki starts here.\n");
    work.write(
        "alpha.md",
        "# Alpha\n\nThe first page talks about boats and harbours.\n",
    );
    work.write("AGENTS.md", "# Agents\n\nHow agents work in this wiki.\n");
    work.write(
        "gotländska minnen.md",
        "# Gotländska minnen\n\nVisby i augusti, med café och ringmur.\n\nEn rad till om sommaren.\n",
    );
    work.write("notes/deep.md", "Deep note without a heading.\n");
    work.write("notes/readme.txt", "Not a page.\n");
    let start = work.commit("Start the wiki");

    work.write(
        "alpha.md",
        "# Alpha\n\nThe first page talks about boats and harbours.\n\nNow with a second line.\n",
    );
    work.commit("Edit alpha");

    work.rename("alpha.md", "greek/alpha.md");
    work.commit("Move alpha");

    work.rename("gotländska minnen.md", "minnen från gotland.md");
    work.write(
        "minnen från gotland.md",
        "# Gotländska minnen\n\nVisby i augusti, med café och ringmur.\n\nEn rad till om vintern.\n",
    );
    work.commit("Rename and edit the memories");

    work.remove("notes/deep.md");
    work.commit("Delete the deep note");

    work.write(
        "notes/deep.md",
        "# Deep again\n\nA new page at the old path.\n",
    );
    work.commit("Recreate the deep note");

    work.write(
        "index.md",
        "# Home\n\nThe wiki starts here.\n\nSee [the harbour](greek/alpha.md).\n",
    );
    work.commit("Link the harbour from home\n\nA writer committed this by hand.");

    work.write(
        "AGENTS.md",
        "# Agents\n\nHow agents work in this wiki, with care.\n\n## Open questions\n\n\
         - Should agents sign their pages?\n- Who reads what an agent *wrote*?\n",
    );
    work.commit(&format!("Save AGENTS.md\n\nOverlap: {start}"));

    work.write("notes/readme.txt", "Still not a page.\n");
    work.commit("Edit a file that is not a page");

    work.write(
        "greek/alpha.md",
        "# Alpha\n\nThe first page talks about boats and harbours.\n\nNow with a second line.\n\nAnd a third.\n",
    );
    work.commit("Edit alpha again");
    work
}
