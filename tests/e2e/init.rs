//! `mneme init`, with a temporary `XDG_CONFIG_HOME` and a `systemctl` on `PATH` that records
//! its arguments.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{Duration, SystemTime};

use mneme::autosave::{Autosave, Login, Outcome, Remote, Settings};
use tempfile::TempDir;

use crate::git::Isolation;
use crate::remote::{bare_wiki, mneme, write_quiet};
use crate::server::{MACHINES, Server};

/// A machine with a home of its own, a `systemctl` that only records what it is asked, and
/// `bin` first on `PATH`.
struct Machine {
    git: Isolation,
    root: TempDir,
}

impl Machine {
    fn new() -> Self {
        let machine = Self {
            git: Isolation::new(),
            root: TempDir::new().unwrap(),
        };
        let log = machine.root().join("systemctl.log");
        let script = format!("#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n", log.display());
        fs::create_dir_all(machine.bin()).unwrap();
        fs::write(machine.bin().join("systemctl"), script).unwrap();
        fs::set_permissions(
            machine.bin().join("systemctl"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        machine
    }

    fn root(&self) -> &Path {
        self.root.path()
    }

    fn bin(&self) -> PathBuf {
        self.root().join("bin")
    }

    /// `mneme init` with `args`, run in the machine's root.
    fn init(&self, args: &[&str]) -> Output {
        mneme(&self.git, &self.bin())
            .current_dir(self.root())
            .arg("init")
            .args(args)
            .output()
            .unwrap()
    }

    /// Where the unit goes under the `XDG_CONFIG_HOME` that [`Isolation`] gives every command.
    fn unit_path(&self) -> PathBuf {
        self.git
            .home()
            .join("xdg/systemd/user/mneme-autosave.service")
    }

    fn unit(&self) -> String {
        fs::read_to_string(self.unit_path()).unwrap()
    }

    /// What `systemctl` was asked, one run per line, since the last call.
    fn systemctl(&self) -> String {
        let log = self.root().join("systemctl.log");
        let said = fs::read_to_string(&log).unwrap_or_default();
        let _ = fs::remove_file(&log);
        said
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[test]
fn init_clones_writes_the_unit_with_absolute_paths_and_starts_the_service() {
    let machine = Machine::new();
    let bare = bare_wiki(&machine.git, machine.root());
    let folder = machine.root().join("my wiki");
    let folder_arg = folder.to_str().unwrap();
    let bare_arg = bare.to_str().unwrap();
    let args = [
        "--folder", folder_arg, "--remote", bare_arg, "--name", "laptop",
    ];

    let output = machine.init(&args);
    assert!(output.status.success(), "{output:?}");
    assert!(folder.join("notes.md").exists(), "the wiki was cloned");
    assert!(
        stdout(&output).starts_with(&format!("Cloned the wiki into {folder_arg}.\n")),
        "{output:?}"
    );
    assert_eq!(stderr(&output), "", "a person reads no log lines");
    assert_eq!(
        machine.systemctl(),
        "--user daemon-reload\n--user enable --now mneme-autosave.service\n"
    );
    assert!(
        stdout(&output).contains("journalctl --user -u mneme-autosave"),
        "{output:?}"
    );
    let unit = machine.unit();
    let exec_start = format!(
        "ExecStart={} autosave run --folder \"{folder_arg}\" --remote {bare_arg} --name laptop\n",
        env!("CARGO_BIN_EXE_mneme")
    );
    assert!(unit.contains(&exec_start), "{unit}");
    assert!(
        unit.contains(&format!("ReadWritePaths=\"{folder_arg}\" {bare_arg}\n")),
        "the service writes the folder and the remote on this machine: {unit}"
    );
    for line in [
        "ProtectSystem=strict",
        "ProtectHome=read-only",
        "NoNewPrivileges=yes",
        "KillMode=mixed",
    ] {
        assert!(unit.lines().any(|found| found == line), "{line}: {unit}");
    }

    let again = machine.init(&args);
    assert!(again.status.success(), "init can run again: {again:?}");
    assert!(
        stdout(&again).starts_with(&format!("{folder_arg} is a clone of the wiki already.\n")),
        "{again:?}"
    );
    assert_eq!(stderr(&again), "");
    assert_eq!(machine.unit(), unit);
    assert_eq!(
        machine.systemctl(),
        "--user daemon-reload\n--user enable --now mneme-autosave.service\n"
    );

    let renamed = [
        "--folder", folder_arg, "--remote", bare_arg, "--name", "desktop",
    ];
    let refused = machine.init(&renamed);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(
        stderr(&refused).contains(machine.unit_path().to_str().unwrap()),
        "the refusal names the unit: {refused:?}"
    );
    assert!(stderr(&refused).contains("--force"), "{refused:?}");
    assert_eq!(machine.unit(), unit, "the unit is left as it was");
    assert_eq!(machine.systemctl(), "");

    let forced = machine.init(&[&renamed[..], &["--force"]].concat());
    assert!(forced.status.success(), "{forced:?}");
    assert!(machine.unit().contains("--name desktop\n"));
    assert_eq!(
        machine.systemctl(),
        "--user daemon-reload\n--user enable --now mneme-autosave.service\n\
         --user try-restart mneme-autosave.service\n",
        "a replaced unit restarts a service that runs the old one"
    );
}

#[test]
fn init_without_a_service_prints_the_command_and_installs_nothing() {
    let machine = Machine::new();
    let bare = bare_wiki(&machine.git, machine.root());
    let folder = machine.root().join("wiki");
    let args = [
        "--folder",
        folder.to_str().unwrap(),
        "--remote",
        bare.to_str().unwrap(),
        "--name",
        "laptop",
    ];
    let output = machine.init(&[&args[..], &["--no-service"]].concat());
    assert!(output.status.success(), "{output:?}");
    let command = format!(
        "{} autosave run --folder {} --remote {} --name laptop",
        env!("CARGO_BIN_EXE_mneme"),
        folder.display(),
        bare.display()
    );
    assert!(stdout(&output).contains(&command), "{output:?}");
    assert!(!machine.unit_path().exists());
    assert_eq!(machine.systemctl(), "");

    let lonely = machine.root().join("only-git");
    fs::create_dir(&lonely).unwrap();
    let git = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .unwrap();
    std::os::unix::fs::symlink(git, lonely.join("git")).unwrap();
    let elsewhere = machine.root().join("elsewhere");
    let output = machine
        .git
        .apply(
            std::process::Command::new(env!("CARGO_BIN_EXE_mneme"))
                .env("PATH", &lonely)
                .args(["init", "--folder"])
                .arg(&elsewhere)
                .args(["--remote", bare.to_str().unwrap(), "--name", "laptop"]),
        )
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert!(
        stdout(&output).contains("systemctl is not on PATH"),
        "{output:?}"
    );
    assert!(
        stdout(&output).contains("autosave run --folder"),
        "{output:?}"
    );
    assert!(elsewhere.join("notes.md").exists());
    assert!(!machine.unit_path().exists());
}

#[test]
fn init_takes_a_folder_of_files_when_the_remote_has_no_main_and_refuses_otherwise() {
    let machine = Machine::new();
    let git = &machine.git;
    let empty = machine.root().join("empty.git");
    git.ok(
        machine.root(),
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            empty.to_str().unwrap(),
        ],
    );
    let folder = machine.root().join("notes");
    fs::create_dir_all(folder.join("old")).unwrap();
    write_quiet(&folder.join("index.md"), "# My notes\n");
    write_quiet(&folder.join("old/trip.md"), "# A trip\n");
    let output = machine.init(&[
        "--folder",
        folder.to_str().unwrap(),
        "--remote",
        empty.to_str().unwrap(),
        "--name",
        "laptop",
        "--no-service",
    ]);
    assert!(output.status.success(), "{output:?}");
    assert!(
        stdout(&output).contains("is a Git repository now, and the service's first save"),
        "{output:?}"
    );
    assert_eq!(stderr(&output), "");
    assert_eq!(
        git.ok(&folder, &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/main"
    );
    assert_eq!(
        git.ok(&folder, &["remote", "get-url", "origin"]).trim(),
        empty.to_str().unwrap()
    );
    assert_eq!(
        fs::read_to_string(folder.join("index.md")).unwrap(),
        "# My notes\n"
    );
    assert!(
        !git.git(&folder, &["rev-parse", "-q", "--verify", "HEAD"])
            .status
            .success(),
        "nothing is committed until the first save"
    );

    let mut service = Autosave::new(Settings {
        folder: folder.clone(),
        remote: Remote::git(empty.to_str().unwrap(), Login::default()).unwrap(),
        machine: "laptop".to_owned(),
        quiet: Duration::from_secs(15),
        fetch_every: Duration::from_secs(30),
        report_every: Duration::from_secs(300),
        stale_lock: Duration::from_secs(300),
        git: "git".into(),
    });
    assert_eq!(service.tick(SystemTime::now()), Outcome::Pushed);
    assert_eq!(git.ok(&empty, &["show", "main:old/trip.md"]), "# A trip\n");

    let other = machine.root().join("other notes");
    fs::create_dir(&other).unwrap();
    fs::write(other.join("mine.md"), "# Mine\n").unwrap();
    let refused = machine.init(&[
        "--folder",
        other.to_str().unwrap(),
        "--remote",
        empty.to_str().unwrap(),
        "--name",
        "desktop",
        "--no-service",
    ]);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(
        stderr(&refused).contains("already has a main"),
        "{refused:?}"
    );
    assert!(stderr(&refused).contains("Move the"), "{refused:?}");
    assert!(!other.join(".git").exists());
    assert_eq!(
        fs::read_to_string(other.join("mine.md")).unwrap(),
        "# Mine\n"
    );
}

#[test]
fn init_puts_the_wiki_in_home_and_names_the_machine_after_the_host() {
    let machine = Machine::new();
    let bare = bare_wiki(&machine.git, machine.root());
    let output = machine.init(&["--remote", bare.to_str().unwrap(), "--no-service"]);
    assert!(output.status.success(), "{output:?}");
    let folder = machine.git.home().join("wiki");
    assert!(folder.join("notes.md").exists(), "{output:?}");
    let host = fs::read_to_string("/proc/sys/kernel/hostname").unwrap();
    let host = host.trim().split('.').next().unwrap();
    assert!(
        stdout(&output).contains(&format!(
            "--folder {} --remote {} --name {host}",
            folder.display(),
            bare.display()
        )),
        "{output:?}"
    );
}

#[test]
fn init_with_a_server_writes_the_absolute_path_of_its_token_file() {
    let machine = Machine::new();
    let server = Server::empty(&machine.git);
    fs::write(machine.root().join("token"), MACHINES[1].1).unwrap();
    let folder = machine.root().join("wiki");
    let output = machine.init(&[
        "--folder",
        folder.to_str().unwrap(),
        "--server",
        &server.url,
        "--token-file",
        "token",
        "--name",
        MACHINES[1].0,
    ]);
    assert!(output.status.success(), "{output:?}");
    assert!(folder.join(".git").is_dir());
    let token = machine.root().join("token");
    assert!(
        machine.unit().contains(&format!(
            "autosave run --folder {} --server {} --token-file {} --name {}\n",
            folder.display(),
            server.url,
            token.display(),
            MACHINES[1].0
        )),
        "{}",
        machine.unit()
    );
    assert!(
        machine
            .unit()
            .contains(&format!("ReadWritePaths={}\n", folder.display())),
        "{}",
        machine.unit()
    );
}

#[test]
fn init_checks_out_main_whatever_the_remotes_default_branch_and_refuses_a_remote_without_it() {
    let machine = Machine::new();
    let git = &machine.git;
    let bare = bare_wiki(git, machine.root());
    let bare_arg = bare.to_str().unwrap();
    let work = machine.root().join("work");
    git.ok(
        machine.root(),
        &["clone", "-q", bare_arg, work.to_str().unwrap()],
    );
    git.ok(&work, &["checkout", "-q", "-b", "master"]);
    fs::write(work.join("old.md"), "# Old\n").unwrap();
    git.ok(&work, &["add", "-A"]);
    git.ok(&work, &["commit", "-q", "-m", "An old branch"]);
    git.ok(&work, &["push", "-q", "origin", "master"]);
    git.ok(&bare, &["symbolic-ref", "HEAD", "refs/heads/master"]);

    let folder = machine.root().join("wiki");
    let output = machine.init(&[
        "--folder",
        folder.to_str().unwrap(),
        "--remote",
        bare_arg,
        "--name",
        "laptop",
        "--no-service",
    ]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        git.ok(&folder, &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/main"
    );
    assert!(folder.join("notes.md").exists());
    assert!(
        !folder.join("old.md").exists(),
        "main is checked out, not master"
    );

    git.ok(&bare, &["update-ref", "-d", "refs/heads/main"]);
    let elsewhere = machine.root().join("elsewhere");
    let refused = machine.init(&[
        "--folder",
        elsewhere.to_str().unwrap(),
        "--remote",
        bare_arg,
        "--name",
        "desktop",
        "--no-service",
    ]);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(
        stderr(&refused).contains("has no main branch, only master"),
        "{refused:?}"
    );
    assert!(!elsewhere.exists(), "nothing is cloned");
}

#[test]
fn init_writes_the_path_of_a_file_url_into_the_units_writable_paths() {
    let machine = Machine::new();
    let git = &machine.git;
    let bare = bare_wiki(git, machine.root());
    let spaced = machine.root().join("the wiki.git");
    fs::rename(&bare, &spaced).unwrap();
    let url = format!(
        "file://localhost{}",
        spaced.to_str().unwrap().replace(' ', "%20")
    );
    let folder = machine.root().join("wiki");
    let output = machine.init(&[
        "--folder",
        folder.to_str().unwrap(),
        "--remote",
        &url,
        "--name",
        "laptop",
    ]);
    assert!(output.status.success(), "{output:?}");
    assert!(folder.join("notes.md").exists());
    let unit = machine.unit();
    assert!(
        unit.contains(&format!(
            "ReadWritePaths={} \"{}\"\n",
            folder.display(),
            spaced.display()
        )),
        "{unit}"
    );

    let elsewhere = machine.root().join("elsewhere");
    let refused = machine.init(&[
        "--folder",
        elsewhere.to_str().unwrap(),
        "--remote",
        &format!("file://example.com{}", spaced.display()),
        "--name",
        "laptop",
        "--force",
    ]);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(
        stderr(&refused).contains("names a host other than localhost"),
        "{refused:?}"
    );
}
