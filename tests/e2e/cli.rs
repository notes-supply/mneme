use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use tempfile::TempDir;

#[test]
fn an_unknown_subcommand_is_refused() {
    let status = Command::new(env!("CARGO_BIN_EXE_mneme"))
        .arg("unknown")
        .output()
        .unwrap()
        .status;
    assert!(!status.success());
}

#[test]
fn autosave_refuses_a_number_of_seconds_that_is_not_one() {
    for value in ["-1", "NaN", "inf", "soon", "1e30"] {
        let output = Command::new(env!("CARGO_BIN_EXE_mneme"))
            .args([
                "autosave",
                "run",
                "--folder",
                ".",
                "--server",
                "http://127.0.0.1:9",
            ])
            .args(["--token-file", "/dev/null", "--name", "x"])
            .arg(format!("--tick-seconds={value}"))
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{value}: {output:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("--tick-seconds"),
            "{value}"
        );
    }
}

/// The real `git` on `PATH`.
fn real_git() -> PathBuf {
    std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join("git"))
        .find(|path| path.is_file())
        .unwrap()
}

/// `mneme autosave <command>` with a `git` first on `PATH` that says it is `version` and passes
/// every other command to the real one.
fn autosave_with_git(dir: &Path, version: &str, command: &str) -> Output {
    let fake = dir.join("bin");
    fs::create_dir_all(&fake).unwrap();
    let script = format!(
        "#!/bin/sh\nif [ \"$1\" = --version ]; then echo '{version}'; exit 0; fi\nexec '{}' \"$@\"\n",
        real_git().display()
    );
    fs::write(fake.join("git"), script).unwrap();
    fs::set_permissions(fake.join("git"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(dir.join("token"), "a-token\n").unwrap();
    let path = std::env::join_paths(
        [fake]
            .into_iter()
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    Command::new(env!("CARGO_BIN_EXE_mneme"))
        .args(["autosave", command, "--folder"])
        .arg(dir.join("wiki"))
        .args(["--server", "http://127.0.0.1:9", "--token-file"])
        .arg(dir.join("token"))
        .args(["--name", "x"])
        .env("PATH", path)
        .output()
        .unwrap()
}

#[test]
fn autosave_refuses_to_start_with_a_git_older_than_2_45() {
    for command in ["init", "run"] {
        for version in ["git version 2.44.2", "git version 2.39.3 (Apple Git-145)"] {
            let dir = TempDir::new().unwrap();
            let output = autosave_with_git(dir.path(), version, command);
            let said = String::from_utf8_lossy(&output.stderr);
            assert!(!output.status.success(), "{command} {version}: {said}");
            let found = version.split(' ').nth(2).unwrap();
            assert!(said.contains(found), "{command} {version}: {said}");
            assert!(said.contains("2.45.0"), "{command} {version}: {said}");
            assert!(
                !dir.path().join("wiki").exists(),
                "{command} {version}: nothing was made"
            );
        }
        for version in ["git version 2.45.0", "git version 2.56.0.windows.1"] {
            let dir = TempDir::new().unwrap();
            let output = autosave_with_git(dir.path(), version, command);
            let said = String::from_utf8_lossy(&output.stderr);
            assert!(
                !said.contains("2.45.0 or newer"),
                "{command} {version}: {said}"
            );
        }
    }
}
