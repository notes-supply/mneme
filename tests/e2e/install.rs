//! `install.sh`, piped to `sh` as the README runs it, against a fake release served on
//! loopback. Its `PATH` holds only the tools it needs, with `uname`, `git`, `systemctl` and
//! `cosign` of the test's own, so nothing outside a temporary home is touched.

use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::thread;

use sha2::{Digest, Sha256};
use tempfile::TempDir;

/// The tag of the fake releases.
const TAG: &str = "v9.9.9";
/// The tools `install.sh` may run from the machine itself.
const TOOLS: &[&str] = &[
    "sh",
    "curl",
    "sha256sum",
    "mktemp",
    "mkdir",
    "rm",
    "cp",
    "chmod",
    "mv",
    "cat",
    "awk",
    "cut",
];

/// The fake binary of `app` for `arch`.
fn binary(app: &str, arch: &str) -> String {
    format!("#!/bin/sh\necho '{app} {arch} {TAG}'\n")
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

fn executable(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

/// A machine with a home of its own, a `PATH` of only `bin`, and a release of each app on a
/// server of its own.
struct Machine {
    root: TempDir,
    url: String,
}

impl Machine {
    fn new(uname_machine: &str) -> Self {
        let root = TempDir::new().unwrap();
        let releases = root.path().join("releases");
        for app in ["mneme", "atrium"] {
            let download = releases.join(app).join("releases/download").join(TAG);
            fs::create_dir_all(&download).unwrap();
            let mut sums = String::new();
            for arch in ["x86_64", "aarch64"] {
                let file = format!("{app}-{arch}-linux");
                fs::write(download.join(&file), binary(app, arch)).unwrap();
                fs::write(download.join(format!("{file}.sigstore.json")), "{}\n").unwrap();
                let _ = writeln!(sums, "{}  {file}", sha256(binary(app, arch).as_bytes()));
            }
            fs::write(download.join("SHA256SUMS"), sums).unwrap();
        }
        let contrib = releases.join("atrium/raw").join(TAG).join("contrib");
        fs::create_dir_all(&contrib).unwrap();
        fs::write(
            contrib.join("atrium.service"),
            "[Unit]\nDescription=Atrium\n",
        )
        .unwrap();

        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        // The system's own tools first, since a tool manager's shim on PATH may not run
        // outside the home it was set up in.
        let path = std::env::var_os("PATH").unwrap();
        let dirs: Vec<PathBuf> = [PathBuf::from("/usr/bin"), PathBuf::from("/bin")]
            .into_iter()
            .chain(std::env::split_paths(&path))
            .collect();
        for tool in TOOLS {
            let found = dirs
                .iter()
                .map(|dir| dir.join(tool))
                .find(|candidate| candidate.is_file())
                .unwrap_or_else(|| panic!("{tool} is not on PATH"));
            std::os::unix::fs::symlink(found, bin.join(tool)).unwrap();
        }
        executable(
            &bin.join("uname"),
            &format!("#!/bin/sh\ncase $1 in -s) echo Linux ;; -m) echo {uname_machine} ;; esac\n"),
        );
        executable(&bin.join("git"), "#!/bin/sh\necho 'git version 2.44.1'\n");
        let systemctl_log = root.path().join("systemctl.log");
        executable(
            &bin.join("systemctl"),
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n",
                systemctl_log.display()
            ),
        );
        fs::create_dir_all(root.path().join("home")).unwrap();
        fs::create_dir(root.path().join("tmp")).unwrap();
        let url = serve(releases);
        Self { root, url }
    }

    fn home(&self) -> PathBuf {
        self.root.path().join("home")
    }

    /// The default install folder.
    fn bin_dir(&self) -> PathBuf {
        self.home().join(".local/bin")
    }

    fn unit(&self) -> PathBuf {
        self.home().join(".config/systemd/user/atrium.service")
    }

    fn download(&self, app: &str, file: &str) -> PathBuf {
        self.root
            .path()
            .join("releases")
            .join(app)
            .join("releases/download")
            .join(TAG)
            .join(file)
    }

    /// A `cosign` on `PATH` that records its arguments and exits with `code`.
    fn cosign(&self, code: i32) -> PathBuf {
        let log = self.root.path().join("cosign.log");
        executable(
            &self.root.path().join("bin/cosign"),
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit {code}\n",
                log.display()
            ),
        );
        log
    }

    /// A `systemctl` that records its arguments and finds no user manager, as over SSH.
    fn without_user_manager(&self) {
        executable(
            &self.root.path().join("bin/systemctl"),
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nexit 1\n",
                self.root.path().join("systemctl.log").display()
            ),
        );
    }

    /// Atrium's unit as the release serves it.
    fn served_unit(&self) -> PathBuf {
        self.root
            .path()
            .join("releases/atrium/raw")
            .join(TAG)
            .join("contrib/atrium.service")
    }

    /// What `systemctl` was asked, one run per line.
    fn systemctl(&self) -> String {
        fs::read_to_string(self.root.path().join("systemctl.log")).unwrap_or_default()
    }

    /// `curl ... | sh -s -- args`, with `env` added.
    fn install(&self, args: &[&str], env: &[(&str, &Path)]) -> Output {
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("install.sh");
        let bin = self.root.path().join("bin");
        let output = Command::new(bin.join("sh"))
            .args(["-s", "--"])
            .args(args)
            .env_clear()
            .env("PATH", &bin)
            .env("HOME", self.home())
            .env("TMPDIR", self.root.path().join("tmp"))
            .env("MNEME_RELEASES_URL", &self.url)
            .envs(env.iter().copied())
            .stdin(File::open(script).unwrap())
            .output()
            .unwrap();
        assert_eq!(
            fs::read_dir(self.root.path().join("tmp")).unwrap().count(),
            0,
            "the downloads are removed: {output:?}"
        );
        output
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Serves `root` on loopback, as GitHub serves releases: `.../releases/latest` redirects to
/// the tag's page, and the rest are files.
fn serve(root: PathBuf) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    thread::spawn(move || {
        for stream in listener.incoming().map_while(Result::ok) {
            let root = root.clone();
            thread::spawn(move || answer(stream, &root));
        }
    });
    url
}

fn answer(mut stream: TcpStream, root: &Path) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let mut words = line.split_whitespace();
    let method = words.next().unwrap_or_default().to_owned();
    let path = words.next().unwrap_or_default().to_owned();
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).unwrap_or(0) == 0 || header.trim().is_empty() {
            break;
        }
    }
    let (status, location, body) = if let Some(app) = path.strip_suffix("/releases/latest") {
        ("302 Found", format!("{app}/releases/tag/{TAG}"), Vec::new())
    } else if path.contains("/releases/tag/") {
        ("200 OK", String::new(), b"a release\n".to_vec())
    } else {
        match fs::read(root.join(path.trim_start_matches('/'))) {
            Ok(bytes) if !path.contains("..") => ("200 OK", String::new(), bytes),
            _ => ("404 Not Found", String::new(), Vec::new()),
        }
    };
    let mut response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n",
        body.len()
    );
    if !location.is_empty() {
        let _ = write!(response, "Location: {location}\r\n");
    }
    response.push_str("\r\n");
    let _ = stream.write_all(response.as_bytes());
    if method != "HEAD" {
        let _ = stream.write_all(&body);
    }
}

#[test]
fn a_good_release_installs_both_apps_and_atriums_unit_without_enabling_it() {
    let machine = Machine::new("x86_64");
    let output = machine.install(&[], &[]);
    assert!(output.status.success(), "{output:?}");
    for app in ["mneme", "atrium"] {
        let installed = machine.bin_dir().join(app);
        assert_eq!(
            fs::read_to_string(&installed).unwrap(),
            binary(app, "x86_64")
        );
        assert_eq!(
            installed.metadata().unwrap().permissions().mode() & 0o777,
            0o755
        );
    }
    assert_eq!(
        fs::read_dir(machine.bin_dir()).unwrap().count(),
        2,
        "nothing else is left in the folder"
    );
    assert_eq!(
        fs::read_to_string(machine.unit()).unwrap(),
        "[Unit]\nDescription=Atrium\n"
    );
    assert_eq!(
        machine.systemctl(),
        "--user show-environment\n--user daemon-reload\n",
        "the unit is loaded and not enabled"
    );
    let said = stdout(&output);
    assert!(
        !said.contains("try-restart"),
        "nothing was replaced: {said}"
    );
    for line in [
        format!(
            "Installed mneme {TAG}, atrium {TAG} in {}.",
            machine.bin_dir().display()
        ),
        "cosign is not on PATH, so only the checksums were checked".to_owned(),
        "#verify-a-download".to_owned(),
        format!("Installed {}, not enabled.", machine.unit().display()),
        format!("{} is not on PATH.", machine.bin_dir().display()),
        "Mneme needs Git 2.45 or newer, and this is Git 2.44.1.".to_owned(),
        "  mneme init --remote ".to_owned(),
        "  systemctl --user enable --now atrium\n".to_owned(),
    ] {
        assert!(said.contains(&line), "{line}: {said}");
    }

    fs::write(machine.unit(), "# My own\n").unwrap();
    let again = machine.install(&["atrium", "mneme", "atrium"], &[]);
    assert!(again.status.success(), "{again:?}");
    assert_eq!(fs::read_to_string(machine.unit()).unwrap(), "# My own\n");
    let said = stdout(&again);
    for line in [
        format!("Installed atrium {TAG}, mneme {TAG} in"),
        "is already there, and was left as it is".to_owned(),
        "goes on with it until it restarts: systemctl --user try-restart mneme-autosave atrium"
            .to_owned(),
    ] {
        assert!(said.contains(&line), "{line}: {said}");
    }
    assert!(!machine.systemctl().contains("enable"));
}

#[test]
fn one_app_for_arm64_goes_into_the_named_folder_and_its_signature_is_checked() {
    let machine = Machine::new("arm64");
    let cosign = machine.cosign(0);
    let dir = machine.root.path().join("elsewhere");
    let output = machine.install(&["mneme"], &[("MNEME_INSTALL_DIR", &dir)]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        fs::read_to_string(dir.join("mneme")).unwrap(),
        binary("mneme", "aarch64")
    );
    assert!(!dir.join("atrium").exists());
    assert!(!machine.bin_dir().exists());
    let checked = fs::read_to_string(cosign).unwrap();
    assert!(
        checked.starts_with("verify-blob ")
            && checked.contains("mneme-aarch64-linux --bundle ")
            && checked.contains("mneme-aarch64-linux.sigstore.json")
            && checked.contains(
                "--certificate-oidc-issuer https://token.actions.githubusercontent.com"
            )
            && checked.contains(&format!(
                "--certificate-identity https://github.com/notes-supply/mneme/.github/workflows/binaries.yml@refs/tags/{TAG}\n"
            )),
        "{checked}"
    );
    let said = stdout(&output);
    assert!(
        said.contains("matched its release's SHA256SUMS and its signature"),
        "{said}"
    );
    assert!(!said.contains("atrium"), "{said}");

    machine.cosign(1);
    let refused = machine.install(&["atrium"], &[("MNEME_INSTALL_DIR", &dir)]);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(stderr(&refused).contains("is not signed by"), "{refused:?}");
    assert!(!dir.join("atrium").exists());

    fs::remove_file(machine.served_unit()).unwrap();
    machine.cosign(0);
    let atrium = machine.install(&["atrium"], &[("MNEME_INSTALL_DIR", &dir)]);
    assert!(
        atrium.status.success(),
        "a folder of its own needs no unit, so a missing one is not fetched: {atrium:?}"
    );
    assert!(dir.join("atrium").exists());
    assert!(
        stdout(&atrium).contains("  atrium --wiki-folder ~/wiki\n"),
        "{atrium:?}"
    );
    assert_eq!(machine.systemctl(), "");
}

#[test]
fn atrium_alone_without_a_user_manager_gets_its_unit_and_is_told_to_install_mneme_first() {
    let machine = Machine::new("x86_64");
    machine.without_user_manager();
    let output = machine.install(&["atrium"], &[]);
    assert!(output.status.success(), "{output:?}");
    assert!(
        machine.unit().exists(),
        "the unit is installed without a user manager"
    );
    assert_eq!(
        machine.systemctl(),
        "--user show-environment\n",
        "and is not loaded"
    );
    let said = stdout(&output);
    for line in [
        "systemd's user manager did not answer, so run systemctl --user daemon-reload before enabling it",
        "| sh -s -- mneme\n",
        "installs Mneme first, which Atrium runs to read the wiki",
    ] {
        assert!(said.contains(line), "{line}: {said}");
    }
    assert!(!said.contains("enable --now"), "{said}");
}

#[test]
fn a_folder_with_a_space_or_a_quote_is_quoted_in_every_command_printed() {
    let machine = Machine::new("x86_64");
    let dir = machine.root.path().join("my tools/it's here");
    let output = machine.install(&["atrium"], &[("MNEME_INSTALL_DIR", &dir)]);
    assert!(output.status.success(), "{output:?}");
    assert!(dir.join("atrium").exists());
    let quoted = format!("'{}'", dir.display().to_string().replace('\'', r"'\''"));
    let said = stdout(&output);
    for line in [
        format!("MNEME_INSTALL_DIR={quoted} sh -s -- mneme\n"),
        format!("export PATH={quoted}:\"$PATH\""),
    ] {
        assert!(said.contains(&line), "{line}: {said}");
    }
    let shell = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "MNEME_INSTALL_DIR={quoted} sh -c 'printf %s \"$MNEME_INSTALL_DIR\"'"
        ))
        .output()
        .unwrap();
    assert_eq!(
        String::from_utf8_lossy(&shell.stdout),
        dir.to_str().unwrap()
    );
}

#[test]
fn a_binary_that_does_not_match_its_checksum_installs_nothing() {
    let machine = Machine::new("x86_64");
    fs::write(
        machine.download("atrium", "atrium-x86_64-linux"),
        "#!/bin/sh\necho tampered\n",
    )
    .unwrap();
    let output = machine.install(&[], &[]);
    assert!(!output.status.success(), "{output:?}");
    assert!(
        stderr(&output).contains("atrium-x86_64-linux of atrium v9.9.9 does not match its SHA256SUMS, so nothing was installed"),
        "{output:?}"
    );
    assert!(
        !machine.bin_dir().join("mneme").exists(),
        "not even the good one"
    );
    assert!(!machine.bin_dir().join("atrium").exists());
    assert!(!machine.unit().exists());
    assert_eq!(machine.systemctl(), "");
}

#[test]
fn an_unknown_architecture_installs_nothing() {
    let machine = Machine::new("riscv64");
    let output = machine.install(&[], &[]);
    assert!(!output.status.success(), "{output:?}");
    assert!(
        stderr(&output).contains("for x86_64 and aarch64 only, and this machine is riscv64"),
        "{output:?}"
    );
    assert!(!machine.bin_dir().exists());
    assert!(!machine.unit().exists());
}
