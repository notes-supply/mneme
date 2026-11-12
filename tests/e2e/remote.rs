//! `mneme autosave` with a plain Git remote: SSH through the user's own `ssh`, and HTTPS with a
//! token as HTTP Basic authentication, against `git http-backend` behind a small test server.

use std::fs::{self, File};
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use mneme::autosave::{Autosave, Login, Outcome, Remote, Settings, Token};
use serde_json::Value;
use tempfile::TempDir;

use crate::git::Isolation;

/// A bare repository on `main` with two pages, under `root`.
pub fn bare_wiki(git: &Isolation, root: &Path) -> PathBuf {
    let bare = root.join("wiki.git");
    git.ok(
        root,
        &["init", "-q", "--bare", "-b", "main", bare.to_str().unwrap()],
    );
    let work = TempDir::new().unwrap();
    git.ok(work.path(), &["init", "-q", "-b", "main"]);
    fs::write(work.path().join("index.md"), "# Home\n").unwrap();
    fs::write(work.path().join("notes.md"), "# Notes\n").unwrap();
    git.ok(work.path(), &["add", "-A"]);
    git.ok(work.path(), &["commit", "-q", "-m", "Start the wiki"]);
    git.ok(
        work.path(),
        &["push", "-q", bare.to_str().unwrap(), "main:main"],
    );
    bare
}

/// `mneme` with `bin` first on `PATH`.
pub fn mneme(git: &Isolation, bin: &Path) -> Command {
    let path = std::env::join_paths(
        [bin.to_owned()]
            .into_iter()
            .chain(std::env::split_paths(&std::env::var_os("PATH").unwrap())),
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_mneme"));
    git.apply(&mut command).env("PATH", path);
    command
}

/// An `ssh` that writes its arguments to `log`, one run per line, and runs the remote command on
/// this machine, as if every host were this one.
pub fn fake_ssh(bin: &Path, log: &Path) {
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nfor last; do :; done\n\
         exec sh -c \"git ${{last#git-}}\"\n",
        log.display()
    );
    fs::create_dir_all(bin).unwrap();
    fs::write(bin.join("ssh"), script).unwrap();
    fs::set_permissions(bin.join("ssh"), fs::Permissions::from_mode(0o755)).unwrap();
}

/// A running `mneme autosave run` and what it has logged.
struct Service {
    child: Child,
    log: Arc<Mutex<Vec<String>>>,
}

impl Service {
    fn start(mut command: Command) -> Self {
        let mut child = command.stderr(Stdio::piped()).spawn().unwrap();
        let stderr = BufReader::new(child.stderr.take().unwrap());
        let log = Arc::new(Mutex::new(Vec::new()));
        let lines = Arc::clone(&log);
        thread::spawn(move || {
            for line in stderr.lines().map_while(Result::ok) {
                lines.lock().unwrap().push(line);
            }
        });
        Self { child, log }
    }

    /// Waits until the service has logged the outcome `outcome`.
    fn wait_for(&self, outcome: &str) {
        let deadline = Instant::now() + Duration::from_secs(30);
        let wanted = format!(r#""outcome":"{outcome}""#);
        while !self
            .log
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains(&wanted))
        {
            assert!(
                Instant::now() < deadline,
                "no {outcome}: {:?}",
                self.log.lock().unwrap()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn terminate(mut self) -> Vec<String> {
        let pid = self.child.id().to_string();
        assert!(
            Command::new("kill")
                .args(["-TERM", &pid])
                .status()
                .unwrap()
                .success()
        );
        assert!(self.child.wait().unwrap().success());
        thread::sleep(Duration::from_millis(50));
        self.log.lock().unwrap().clone()
    }
}

/// Writes a page dated a minute back, so that the service finds it quiet at once.
pub fn write_quiet(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_modified(SystemTime::now() - Duration::from_secs(60))
        .unwrap();
}

#[test]
fn an_ssh_remote_runs_the_users_ssh_with_no_prompt_and_learns_a_host_key_only_at_init() {
    let git = Isolation::new();
    let root = TempDir::new().unwrap();
    let bare = bare_wiki(&git, root.path());
    let bin = root.path().join("bin");
    let ssh_log = root.path().join("ssh.log");
    fake_ssh(&bin, &ssh_log);
    let key = root.path().join("deploy key");
    fs::write(&key, "not really a key\n").unwrap();
    let folder = root.path().join("wiki");
    let url = format!("localhost:{}", bare.display());
    let args = |command: &str| {
        let mut args: Vec<String> = ["autosave", command, "--folder"].map(str::to_owned).into();
        args.extend([
            folder.to_str().unwrap().to_owned(),
            "--remote".to_owned(),
            url.clone(),
            "--ssh-key".to_owned(),
            key.to_str().unwrap().to_owned(),
            "--name".to_owned(),
            "laptop".to_owned(),
        ]);
        args
    };

    let init = mneme(&git, &bin).args(args("init")).output().unwrap();
    assert!(init.status.success(), "{init:?}");
    assert!(folder.join("notes.md").exists(), "init cloned the wiki");
    assert_eq!(
        git.ok(&folder, &["remote", "get-url", "origin"]).trim(),
        url
    );
    let runs = fs::read_to_string(&ssh_log).unwrap();
    let key_option = format!("-i {} -o IdentitiesOnly=yes", key.display());
    assert!(
        runs.lines().all(|run| run.starts_with("-o BatchMode=yes ")
            && run.contains(&key_option)
            && run.contains("-o StrictHostKeyChecking=accept-new")),
        "{runs}"
    );
    fs::remove_file(&ssh_log).unwrap();

    let mut run = mneme(&git, &bin);
    run.args(args("run")).args([
        "--tick-seconds",
        "0.1",
        "--quiet-seconds",
        "0.5",
        "--fetch-seconds",
        "0.5",
    ]);
    let service = Service::start(run);
    write_quiet(&folder.join("notes.md"), "# Notes\n\nSaved over SSH.\n");
    service.wait_for("pushed");
    let log = service.terminate();
    let pushed = git.ok(&bare, &["show", "main:notes.md"]);
    assert_eq!(pushed, "# Notes\n\nSaved over SSH.\n");
    assert_eq!(
        git.ok(&bare, &["log", "-1", "--format=%an <%ae>", "main"])
            .trim(),
        "laptop <autosave@laptop>"
    );
    let runs = fs::read_to_string(&ssh_log).unwrap();
    assert!(!runs.is_empty(), "the service fetched and pushed with ssh");
    assert!(
        runs.lines().all(|run| run.starts_with("-o BatchMode=yes ")
            && run.contains(&key_option)
            && !run.contains("StrictHostKeyChecking")),
        "the service never accepts a new host key: {runs}"
    );
    let started: Value = log
        .iter()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|line| line["event"] == "started")
        .unwrap();
    assert_eq!(started["remote"], url.as_str());
    assert_eq!(started["server"], Value::Null);
}

/// `git http-backend` over plain HTTP on a free port, behind a check of the `Authorization`
/// header. It records every request's header.
struct Backend {
    url: String,
    seen: Arc<Mutex<Vec<Option<String>>>>,
}

impl Backend {
    fn start(git: &Isolation, root: &Path, authorization: String) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&seen);
        let root = root.to_owned();
        let mut cgi = Command::new("git");
        git.apply(&mut cgi);
        let env: Vec<(String, String)> = cgi
            .get_envs()
            .filter_map(|(key, value)| {
                Some((key.to_str()?.to_owned(), value?.to_str()?.to_owned()))
            })
            .collect();
        thread::spawn(move || {
            for stream in listener.incoming().map_while(Result::ok) {
                let recorded = Arc::clone(&recorded);
                let (root, env, authorization) = (root.clone(), env.clone(), authorization.clone());
                thread::spawn(move || serve(stream, &root, &env, &authorization, &recorded));
            }
        });
        Self { url, seen }
    }
}

/// One HTTP request as the test server reads it.
struct Request {
    method: String,
    target: String,
    /// Names in lower case.
    headers: Vec<(String, String)>,
}

impl Request {
    fn read(reader: &mut impl BufRead) -> Self {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let mut parts = line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_owned();
        let target = parts.next().unwrap_or_default().to_owned();
        let mut headers = Vec::new();
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).unwrap();
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            let (name, value) = header.split_once(':').unwrap();
            headers.push((name.to_ascii_lowercase(), value.trim().to_owned()));
        }
        Self {
            method,
            target,
            headers,
        }
    }

    fn header(&self, name: &str) -> Option<String> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.clone())
    }

    /// The body, by its length or in chunks.
    fn body(&self, reader: &mut impl BufRead) -> Vec<u8> {
        let mut body = Vec::new();
        if let Some(length) = self.header("content-length") {
            body.resize(length.parse().unwrap(), 0);
            reader.read_exact(&mut body).unwrap();
        } else if self.header("transfer-encoding").as_deref() == Some("chunked") {
            loop {
                let mut size = String::new();
                reader.read_line(&mut size).unwrap();
                let size = usize::from_str_radix(size.trim(), 16).unwrap();
                let mut chunk = vec![0; size + 2];
                reader.read_exact(&mut chunk).unwrap();
                if size == 0 {
                    break;
                }
                body.extend_from_slice(&chunk[..size]);
            }
        }
        body
    }
}

/// Answers one request on `stream`, then closes it.
fn serve(
    mut stream: TcpStream,
    root: &Path,
    env: &[(String, String)],
    authorization: &str,
    seen: &Mutex<Vec<Option<String>>>,
) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let request = Request::read(&mut reader);
    let given = request.header("authorization");
    seen.lock().unwrap().push(given.clone());
    if given.as_deref() != Some(authorization) {
        let _ = stream.write_all(
            b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=\"wiki\"\r\n\
              Content-Length: 0\r\nConnection: close\r\n\r\n",
        );
        return;
    }
    let body = request.body(&mut reader);
    let _ = stream.write_all(&http_backend(&request, &body, root, env));
}

/// Runs `git http-backend` as a CGI program for `request` and returns its HTTP response.
fn http_backend(request: &Request, body: &[u8], root: &Path, env: &[(String, String)]) -> Vec<u8> {
    let (path, query) = request
        .target
        .split_once('?')
        .unwrap_or((&request.target, ""));
    let mut cgi = Command::new("git");
    cgi.arg("http-backend")
        .envs(env.iter().map(|(key, value)| (key, value)))
        .env("GIT_PROJECT_ROOT", root)
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("REMOTE_USER", "writer")
        .env("REMOTE_ADDR", "127.0.0.1")
        .env("REQUEST_METHOD", &request.method)
        .env("PATH_INFO", path)
        .env("QUERY_STRING", query)
        .env("CONTENT_LENGTH", body.len().to_string())
        .env(
            "CONTENT_TYPE",
            request.header("content-type").unwrap_or_default(),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(encoding) = request.header("content-encoding") {
        cgi.env("HTTP_CONTENT_ENCODING", encoding);
    }
    if let Some(protocol) = request.header("git-protocol") {
        cgi.env("GIT_PROTOCOL", protocol);
    }
    let mut child = cgi.spawn().unwrap();
    child.stdin.take().unwrap().write_all(body).unwrap();
    let output = child.wait_with_output().unwrap();
    let (end, gap) = output
        .stdout
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|at| (at, 4))
        .or_else(|| {
            output
                .stdout
                .windows(2)
                .position(|window| window == b"\n\n")
                .map(|at| (at, 2))
        })
        .unwrap();
    let head = String::from_utf8_lossy(&output.stdout[..end]).into_owned();
    let content = &output.stdout[end + gap..];
    let mut status = "200 OK".to_owned();
    let mut headers = String::new();
    for header in head.lines().map(str::trim_end) {
        if let Some(("Status", value)) = header.split_once(": ") {
            value.clone_into(&mut status);
        } else {
            headers.push_str(header);
            headers.push_str("\r\n");
        }
    }
    let mut response = format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
        content.len()
    )
    .into_bytes();
    response.extend_from_slice(content);
    response
}

fn init_https(git: &Isolation, url: &str, token_file: &Path, folder: &Path) -> Output {
    git.apply(
        Command::new(env!("CARGO_BIN_EXE_mneme"))
            .args(["autosave", "init", "--folder"])
            .arg(folder)
            .args(["--remote", url, "--token-file"])
            .arg(token_file)
            .args(["--name", "laptop"]),
    )
    .output()
    .unwrap()
}

#[test]
fn an_https_remote_gets_the_token_as_basic_authentication_and_nowhere_else() {
    let git = Isolation::new();
    let root = TempDir::new().unwrap();
    let bare = bare_wiki(&git, root.path());
    let token = "deploy-token-of-laptop";
    let token_file = root.path().join("token");
    fs::write(&token_file, format!("{token}\n")).unwrap();
    let expected = format!("Basic {}", STANDARD.encode(format!("git:{token}")));
    let backend = Backend::start(&git, root.path(), expected.clone());
    let url = format!("{}/wiki.git", backend.url);

    let folder = root.path().join("wiki");
    let init = init_https(&git, &url, &token_file, &folder);
    assert!(init.status.success(), "{init:?}");
    assert!(folder.join("notes.md").exists(), "init cloned the wiki");
    let config = fs::read_to_string(folder.join(".git/config")).unwrap();
    assert!(!config.contains(token), "the token is not in the config");
    assert!(
        !String::from_utf8_lossy(&init.stderr).contains(token),
        "the token is not in the log"
    );

    let mut service = Autosave::new(Settings {
        folder: folder.clone(),
        remote: Remote::git(
            &url,
            Login {
                token: Some(Token(token.to_owned())),
                ..Login::default()
            },
        )
        .unwrap(),
        machine: "laptop".to_owned(),
        quiet: Duration::from_secs(15),
        fetch_every: Duration::from_secs(30),
        report_every: Duration::from_secs(300),
        stale_lock: Duration::from_secs(300),
        git: "git".into(),
    });
    write_quiet(&folder.join("notes.md"), "# Notes\n\nSaved over HTTPS.\n");
    assert_eq!(service.tick(SystemTime::now()), Outcome::Pushed);
    assert_eq!(
        git.ok(&bare, &["show", "main:notes.md"]),
        "# Notes\n\nSaved over HTTPS.\n"
    );
    let seen = backend.seen.lock().unwrap().clone();
    assert!(
        seen.len() >= 3,
        "init cloned, and the tick fetched and pushed"
    );
    assert!(
        seen.iter()
            .all(|given| given.as_deref() == Some(expected.as_str())),
        "every request carried the token as Basic authentication: {seen:?}"
    );

    let other = root.path().join("refused");
    fs::write(&token_file, "not-the-token\n").unwrap();
    let started = Instant::now();
    let refused = init_https(&git, &url, &token_file, &other);
    assert!(!refused.status.success(), "{refused:?}");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "a refused token fails at once instead of waiting for a password"
    );
}

#[test]
fn an_https_remote_with_a_user_name_sends_that_name() {
    let git = Isolation::new();
    let root = TempDir::new().unwrap();
    bare_wiki(&git, root.path());
    let token_file = root.path().join("token");
    fs::write(&token_file, "s3cret\n").unwrap();
    let expected = format!("Basic {}", STANDARD.encode("alice:s3cret"));
    let backend = Backend::start(&git, root.path(), expected);
    let url = backend.url.replacen("http://", "http://alice@", 1) + "/wiki.git";
    let folder = root.path().join("wiki");
    let init = init_https(&git, &url, &token_file, &folder);
    assert!(init.status.success(), "{init:?}");
    assert!(folder.join("notes.md").exists());
}
