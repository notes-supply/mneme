//! `mneme init`: takes a machine from nothing to syncing. It makes the folder ready as
//! `mneme autosave init` does, writes a systemd user unit that runs the service with absolute
//! paths, and starts it.

use std::ffi::OsString;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};

use crate::autosave::{self, Remote, RemoteArgs, Settings, Transport};

/// The unit's name, and its file's.
const UNIT: &str = "mneme-autosave.service";
/// The unit as a template. Each `@NAME@` is filled in.
const TEMPLATE: &str = include_str!("mneme-autosave.service");

/// Where `mneme init` puts the wiki, where the wiki syncs, and whether a service is installed.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// The wiki folder. The default is `~/wiki`.
    #[arg(long)]
    pub folder: Option<PathBuf>,
    #[command(flatten)]
    pub remote: RemoteArgs,
    /// This machine's name: on every autosave commit, and in a server's tokens file. The
    /// default is the short host name.
    #[arg(long)]
    pub name: Option<String>,
    /// Make the folder ready and print the command that runs the service, instead of
    /// installing a systemd unit.
    #[arg(long)]
    pub no_service: bool,
    /// Replace a unit that is already installed and differs.
    #[arg(long)]
    pub force: bool,
}

/// Makes the folder ready, then installs and starts the service, or prints how to run it.
///
/// # Errors
///
/// Fails when the folder cannot be made ready, a different unit is installed and `--force`
/// was not given, or `systemctl` fails.
pub fn run(args: &Args) -> Result<()> {
    let home = home()?;
    let folder = absolute(&args.folder.clone().unwrap_or_else(|| home.join("wiki")))?;
    let name = match &args.name {
        Some(name) => name.clone(),
        None => short_host_name()?,
    };
    let remote = args.remote.remote(true)?;
    let command = service_command(args, &folder, &name, &remote)?;
    let systemctl = on_path("systemctl");
    let unit = match &systemctl {
        Some(systemctl) if !args.no_service => {
            let path = config_home(&home).join("systemd/user").join(UNIT);
            let text = unit(&command, &folder, &remote)?;
            if !args.force && fs::read_to_string(&path).is_ok_and(|installed| installed != text) {
                bail!(
                    "{} is already there and differs from the unit mneme init would write. \
                     Pass --force to replace it",
                    path.display()
                );
            }
            Some((systemctl, path, text))
        }
        _ => None,
    };
    autosave::prepare(&Settings {
        folder: folder.clone(),
        remote,
        machine: name,
        quiet: Duration::from_secs(15),
        fetch_every: Duration::from_secs(30),
        report_every: Duration::from_secs(300),
        stale_lock: Duration::from_secs(300),
        git: PathBuf::from("git"),
    })?;
    println!("{} is ready.", folder.display());
    let Some((systemctl, path, text)) = unit else {
        if systemctl.is_none() {
            println!("systemctl is not on PATH, so no service is installed.");
        }
        let words: Vec<String> = command.iter().map(|word| shell_word(word)).collect();
        println!("Run the service with:\n\n    {}", words.join(" "));
        return Ok(());
    };
    let replaced = install(&path, &text)?;
    systemctl_user(systemctl, &["daemon-reload"])?;
    systemctl_user(systemctl, &["enable", "--now", UNIT])?;
    if replaced {
        systemctl_user(systemctl, &["try-restart", UNIT])?;
    }
    println!(
        "Installed {} and started it. Its logs: journalctl --user -u mneme-autosave",
        path.display()
    );
    Ok(())
}

fn home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .context("HOME is not set")
}

/// `$XDG_CONFIG_HOME`, or `~/.config` when it is unset or not an absolute path.
fn config_home(home: &Path) -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .unwrap_or_else(|| home.join(".config"))
}

fn absolute(path: &Path) -> Result<PathBuf> {
    std::path::absolute(path).with_context(|| format!("finding {}", path.display()))
}

/// The host name up to its first dot, as systemd's `%l` gives it.
fn short_host_name() -> Result<String> {
    let name = fs::read_to_string("/proc/sys/kernel/hostname")
        .context("reading the host name. Pass --name")?;
    let name = name.trim().split('.').next().unwrap_or_default();
    if name.is_empty() {
        bail!("the host name is empty. Pass --name");
    }
    Ok(name.to_owned())
}

/// The first executable `program` on `PATH`.
fn on_path(program: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|path| {
            path.metadata().is_ok_and(|metadata| {
                metadata.is_file() && metadata.permissions().mode() & 0o111 != 0
            })
        })
}

/// `mneme autosave run` with every path absolute, as the unit runs it.
fn service_command(args: &Args, folder: &Path, name: &str, remote: &Remote) -> Result<Vec<String>> {
    let exe = std::env::current_exe().context("finding the mneme binary")?;
    let mut command = vec![
        utf8(exe.into_os_string())?,
        "autosave".to_owned(),
        "run".to_owned(),
        "--folder".to_owned(),
        utf8(folder.as_os_str().to_owned())?,
    ];
    match remote {
        Remote::Server { url, .. } => command.extend(["--server".to_owned(), url.clone()]),
        Remote::Git { url, .. } => command.extend(["--remote".to_owned(), url.clone()]),
    }
    if let Some(key) = &args.remote.ssh_key {
        command.extend([
            "--ssh-key".to_owned(),
            utf8(absolute(key)?.into_os_string())?,
        ]);
    }
    if let Some(token) = &args.remote.token_file {
        command.extend([
            "--token-file".to_owned(),
            utf8(absolute(token)?.into_os_string())?,
        ]);
    }
    command.extend(["--name".to_owned(), name.to_owned()]);
    Ok(command)
}

fn utf8(text: OsString) -> Result<String> {
    text.into_string()
        .map_err(|text| anyhow::anyhow!("{} is not UTF-8", text.display()))
}

/// The unit for `command`. The service may write the folder, and a remote that is a path on
/// this machine, and nothing else.
fn unit(command: &[String], folder: &Path, remote: &Remote) -> Result<String> {
    let mut writable = vec![folder.to_owned()];
    if let Remote::Git { url, .. } = remote
        && Transport::of(url) == Transport::Local
    {
        writable.push(PathBuf::from(url.strip_prefix("file://").unwrap_or(url)));
    }
    let writable = writable
        .iter()
        .map(|path| path_word(path))
        .collect::<Result<Vec<_>>>()?;
    let command = command
        .iter()
        .map(|word| exec_word(word))
        .collect::<Result<Vec<_>>>()?;
    let description = utf8(folder.as_os_str().to_owned())?;
    if description.chars().any(char::is_control) {
        bail!("the folder's path holds a control character");
    }
    Ok(TEMPLATE
        .replace("@FOLDER@", &description.replace('%', "%%"))
        .replace("@COMMAND@", &command.join(" "))
        .replace("@WRITABLE@", &writable.join(" ")))
}

/// Writes the unit, unless it is there already with the same text. Returns whether it replaced
/// a different one, which a running service still runs until it restarts.
fn install(path: &Path, text: &str) -> Result<bool> {
    let replaced = match fs::read_to_string(path) {
        Ok(installed) if installed == text => return Ok(false),
        Ok(_) => true,
        Err(error) if error.kind() == ErrorKind::NotFound => false,
        Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
    };
    let folder = path.parent().context("the unit has no folder")?;
    fs::create_dir_all(folder).with_context(|| format!("creating {}", folder.display()))?;
    fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;
    Ok(replaced)
}

fn systemctl_user(systemctl: &Path, args: &[&str]) -> Result<()> {
    let output = Command::new(systemctl)
        .arg("--user")
        .args(args)
        .output()
        .with_context(|| format!("running systemctl --user {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "systemctl --user {} failed: {}. With --no-service, mneme init prints the command \
             that runs the service instead",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

/// Whether `word` needs no quoting, for systemd or for a shell.
fn plain(word: &str) -> bool {
    !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-:@+=,".contains(c))
}

/// One word of `ExecStart`: quoted when it needs to be, with `%` and `$` doubled, since systemd
/// reads them as specifiers and variables even inside quotes.
fn exec_word(word: &str) -> Result<String> {
    if word.chars().any(char::is_control) {
        bail!("{word:?} holds a control character, which a unit cannot carry");
    }
    let quoted = if plain(word) {
        word.to_owned()
    } else {
        format!("\"{}\"", word.replace('\\', r"\\").replace('"', "\\\""))
    };
    Ok(quoted.replace('%', "%%").replace('$', "$$"))
}

/// One path of `ReadWritePaths`, which systemd unquotes but does not unescape, so a path with a
/// quote or a backslash cannot be written there.
fn path_word(path: &Path) -> Result<String> {
    let text = path
        .to_str()
        .with_context(|| format!("{} is not UTF-8", path.display()))?;
    if text
        .chars()
        .any(|c| c.is_control() || c == '"' || c == '\\')
    {
        bail!(
            "{} holds a quote, a backslash or a control character, which a unit's \
             ReadWritePaths cannot carry",
            path.display()
        );
    }
    let text = text.replace('%', "%%");
    Ok(if plain(&text) {
        text
    } else {
        format!("\"{text}\"")
    })
}

/// `word` as one word for a POSIX shell.
fn shell_word(word: &str) -> String {
    if plain(word) {
        word.to_owned()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_word_of_exec_start_is_quoted_and_escaped_for_systemd() {
        for (word, written) in [
            ("/usr/bin/mneme", "/usr/bin/mneme"),
            ("ssh://host/~/wiki.git", "\"ssh://host/~/wiki.git\""),
            ("/home/a b/wiki", "\"/home/a b/wiki\""),
            ("100%", "\"100%%\""),
            ("$HOME", "\"$$HOME\""),
            (r#"say "hi" \o/"#, r#""say \"hi\" \\o/""#),
            ("", "\"\""),
        ] {
            assert_eq!(exec_word(word).unwrap(), written, "{word}");
        }
        assert!(exec_word("two\nlines").is_err());
    }

    #[test]
    fn a_path_of_read_write_paths_is_quoted_or_refused() {
        let word = |path: &str| path_word(Path::new(path));
        assert_eq!(word("/home/u/wiki").unwrap(), "/home/u/wiki");
        assert_eq!(word("/home/u/my wiki").unwrap(), "\"/home/u/my wiki\"");
        assert_eq!(word("/home/u/100%").unwrap(), "\"/home/u/100%%\"");
        for path in [
            "/home/u/\"quoted\"",
            "/home/u/back\\slash",
            "/home/u/new\nline",
        ] {
            assert!(word(path).is_err(), "{path:?}");
        }
    }

    #[test]
    fn a_word_for_the_shell_is_quoted_when_it_needs_to_be() {
        assert_eq!(shell_word("/usr/bin/mneme"), "/usr/bin/mneme");
        assert_eq!(shell_word("/home/a b"), "'/home/a b'");
        assert_eq!(shell_word("it's"), r"'it'\''s'");
        assert_eq!(
            shell_word("ssh://host/~/wiki.git"),
            "'ssh://host/~/wiki.git'"
        );
    }

    #[test]
    fn the_template_has_every_placeholder_and_no_other() {
        let unit = TEMPLATE
            .replace("@FOLDER@", "")
            .replace("@COMMAND@", "")
            .replace("@WRITABLE@", "");
        assert!(!unit.contains('@'), "{unit}");
        for line in [
            "NoNewPrivileges=yes",
            "ProtectSystem=strict",
            "ProtectHome=read-only",
            "KillMode=mixed",
            "WantedBy=default.target",
        ] {
            assert!(TEMPLATE.lines().any(|found| found == line), "{line}");
        }
    }
}
