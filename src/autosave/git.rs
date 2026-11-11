//! Runs `git` in the wiki folder, cut off from the user's own Git configuration so that a
//! global gitignore, a signing key, a credential helper or a prompt can never change what is
//! saved or make the service wait.

use std::ffi::OsString;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};

use super::Config;

/// The id Git reads as no object at all.
pub(super) const NO_OBJECT: &str = "0000000000000000000000000000000000000000";

/// One `git` command's result.
pub(super) struct Run {
    pub success: bool,
    /// `None` when a signal ended the command.
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl Run {
    /// The last line Git wrote to standard error, which is where it says what went wrong.
    pub fn error(&self) -> String {
        self.stderr
            .lines()
            .map(str::trim)
            .rfind(|line| !line.is_empty())
            .unwrap_or("git failed without saying why")
            .to_owned()
    }
}

pub(super) struct Git {
    program: PathBuf,
    folder: PathBuf,
    /// The repository inside the folder, `.git`.
    repository: PathBuf,
    /// The server's base URL, which every setting for it is scoped to.
    server: String,
    /// The server's Git repository.
    url: String,
    /// Variables every command gets.
    env: Vec<(OsString, OsString)>,
    /// Configuration every command gets through `GIT_CONFIG_*`. The token is among it, never
    /// on a command line.
    settings: Vec<(String, String)>,
    /// The tick's time, as Git reads a date.
    date: String,
}

impl Git {
    pub fn new(config: &Config) -> Self {
        let folder = config.folder.clone();
        let repository = folder.join(".git");
        let settings = [
            (
                format!("http.{}.extraHeader", config.server),
                format!("Authorization: Bearer {}", config.token.0),
            ),
            ("credential.helper".to_owned(), String::new()),
            ("commit.gpgSign".to_owned(), "false".to_owned()),
            ("core.excludesFile".to_owned(), "/dev/null".to_owned()),
            ("core.attributesFile".to_owned(), "/dev/null".to_owned()),
            ("core.fsmonitor".to_owned(), "false".to_owned()),
            ("gc.autoDetach".to_owned(), "false".to_owned()),
            ("maintenance.autoDetach".to_owned(), "false".to_owned()),
        ];
        let mut env: Vec<(OsString, OsString)> = vec![
            ("GIT_DIR".into(), repository.clone().into()),
            ("GIT_WORK_TREE".into(), folder.clone().into()),
        ];
        env.extend(
            [
                ("GIT_CONFIG_GLOBAL", "/dev/null".to_owned()),
                ("GIT_CONFIG_NOSYSTEM", "1".to_owned()),
                ("GIT_TERMINAL_PROMPT", "0".to_owned()),
                ("GIT_ASKPASS", String::new()),
                ("GIT_EDITOR", "true".to_owned()),
                ("GIT_OPTIONAL_LOCKS", "0".to_owned()),
                ("GIT_LITERAL_PATHSPECS", "1".to_owned()),
                ("LC_ALL", "C".to_owned()),
                ("GIT_COMMITTER_NAME", config.machine.clone()),
                ("GIT_COMMITTER_EMAIL", email(&config.machine)),
            ]
            .map(|(key, value)| (key.into(), value.into())),
        );
        Self {
            program: config.git.clone(),
            folder,
            repository,
            server: config.server.clone(),
            url: format!("{}/git/wiki.git", config.server),
            env,
            settings: settings.into(),
            date: String::new(),
        }
    }

    /// The server's Git repository, as `origin` should name it.
    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn git_dir(&self) -> &Path {
        &self.repository
    }

    /// The wiki folder, the work tree.
    pub fn folder(&self) -> &Path {
        &self.folder
    }

    /// Dates the commits of the coming commands at `now`.
    pub fn set_now(&mut self, now: SystemTime) {
        let secs = now
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_secs());
        self.date = format!("@{secs} +0000");
    }

    pub fn date(&self) -> &str {
        &self.date
    }

    /// `git` with every variable and setting, `extra` settings after them, and none of the
    /// caller's own `GIT_` variables.
    fn command(&self, extra: &[(String, String)]) -> Command {
        let mut command = Command::new(&self.program);
        for (key, _) in std::env::vars_os() {
            if key.to_string_lossy().starts_with("GIT_") {
                command.env_remove(key);
            }
        }
        let settings: Vec<_> = self.settings.iter().chain(extra).collect();
        command
            .envs(self.env.iter().map(|(key, value)| (key, value)))
            .env("GIT_COMMITTER_DATE", &self.date)
            .env("GIT_CONFIG_COUNT", settings.len().to_string())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (n, (key, value)) in settings.into_iter().enumerate() {
            command.env(format!("GIT_CONFIG_KEY_{n}"), key);
            command.env(format!("GIT_CONFIG_VALUE_{n}"), value);
        }
        command
    }

    /// Clones the server's repository into the folder, which must be missing or empty.
    pub fn clone_server(&self) -> Result<Run> {
        let mut command = self.command(&[]);
        command
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .args(["clone", "--quiet", &self.url])
            .arg(&self.folder);
        finish(command, &["clone"], &[])
    }

    /// Asks the server for its `main` with `header` on every request. It needs no repository,
    /// so it works when the folder is not one.
    pub fn ask_server_with(&self, header: &str) -> Result<Run> {
        let extra = [
            (
                format!("http.{}.extraHeader", self.server),
                header.to_owned(),
            ),
            ("protocol.version".to_owned(), "0".to_owned()),
        ];
        let mut command = self.command(&extra);
        command
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .current_dir("/")
            .args(["ls-remote", &self.url, "refs/heads/main"]);
        finish(command, &["ls-remote"], &[])
    }

    /// Runs `git` with `args`, `extra` variables and `input` on standard input.
    pub fn run_with(&self, args: &[&str], extra: &[(&str, &str)], input: &[u8]) -> Result<Run> {
        let mut command = self.command(&[]);
        command
            .current_dir(&self.folder)
            .args(args)
            .envs(extra.iter().copied());
        finish(command, args, input)
    }

    pub fn run(&self, args: &[&str]) -> Result<Run> {
        self.run_with(args, &[], &[])
    }

    /// Standard output of a command that must succeed.
    pub fn ok_with(&self, args: &[&str], extra: &[(&str, &str)], input: &[u8]) -> Result<Vec<u8>> {
        let run = self.run_with(args, extra, input)?;
        if !run.success {
            bail!("git {} failed: {}", args.join(" "), run.error());
        }
        Ok(run.stdout)
    }

    pub fn ok(&self, args: &[&str]) -> Result<Vec<u8>> {
        self.ok_with(args, &[], &[])
    }

    /// The first line of a command that must succeed, such as a commit id.
    pub fn line(&self, args: &[&str]) -> Result<String> {
        let stdout = self.ok(args)?;
        Ok(String::from_utf8_lossy(&stdout).trim().to_owned())
    }
}

fn finish(mut command: Command, args: &[&str], input: &[u8]) -> Result<Run> {
    let mut child = command
        .spawn()
        .with_context(|| format!("starting git {}", args.join(" ")))?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(input)?;
    }
    let output = child.wait_with_output()?;
    Ok(Run {
        success: output.status.success(),
        code: output.status.code(),
        stdout: output.stdout,
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

/// The email of autosave commits from `machine`.
pub(super) fn email(machine: &str) -> String {
    format!("autosave@{machine}")
}
