//! The real `mneme serve` on a free port, and plain HTTP to it.

use std::fmt::Write as _;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::Value;
use tempfile::TempDir;

use crate::git::{Isolation, Work};

/// The token of the machine `laptop`.
pub const TOKEN: &str = "test-token-of-laptop";
pub const AUTHORIZATION: &str = "Authorization: Bearer test-token-of-laptop";
/// Every machine the test servers know, with its token.
pub const MACHINES: [(&str, &str); 3] = [
    ("desktop", "test-token-of-desktop"),
    ("laptop", TOKEN),
    ("tablet", "test-token-of-tablet"),
];

pub struct Server {
    child: Child,
    pub url: String,
    repo: PathBuf,
    log: Arc<Mutex<Vec<String>>>,
    reader: Option<thread::JoinHandle<()>>,
    dirs: Vec<TempDir>,
}

pub struct Reply {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl Reply {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|error| panic!("{error}: {}", self.body))
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// An HTTP/1.1 response read to the end of the connection.
    fn parse(raw: &[u8]) -> Self {
        let split = raw
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .expect("a response with headers");
        let head = String::from_utf8(raw[..split].to_vec()).unwrap();
        let mut lines = head.split("\r\n");
        let status = lines
            .next()
            .unwrap()
            .split(' ')
            .nth(1)
            .unwrap()
            .parse()
            .unwrap();
        let headers: Vec<(String, String)> = lines
            .map(|line| {
                let (key, value) = line.split_once(':').unwrap();
                (key.to_owned(), value.trim().to_owned())
            })
            .collect();
        let mut body = raw[split + 4..].to_vec();
        let chunked = headers.iter().any(|(key, value)| {
            key.eq_ignore_ascii_case("transfer-encoding") && value.eq_ignore_ascii_case("chunked")
        });
        if chunked {
            body = dechunk(&body);
        }
        Self {
            status,
            headers,
            body: String::from_utf8_lossy(&body).into_owned(),
        }
    }
}

/// The body of a chunked response.
fn dechunk(mut raw: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    loop {
        let end = raw.windows(2).position(|pair| pair == b"\r\n").unwrap();
        let size = std::str::from_utf8(&raw[..end]).unwrap();
        let size = usize::from_str_radix(size.split(';').next().unwrap().trim(), 16).unwrap();
        if size == 0 {
            return body;
        }
        body.extend_from_slice(&raw[end + 2..end + 2 + size]);
        raw = &raw[end + 2 + size + 2..];
    }
}

impl Server {
    /// Serves a bare clone of `work`.
    pub fn of(work: &Work) -> Self {
        let bare = TempDir::new().unwrap();
        let path = bare.path().to_str().unwrap();
        work.ok(&["clone", "-q", "--bare", ".", path]);
        let path = bare.path().to_owned();
        Self::spawn(path, &work.git, vec![bare])
    }

    /// Serves a repository that does not exist yet, so the server creates it.
    pub fn empty(git: &Isolation) -> Self {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("wiki.git");
        Self::spawn(path, git, vec![dir])
    }

    /// A second server on this one's repository, which builds its index afresh.
    pub fn again(&self, git: &Isolation) -> Self {
        Self::spawn(self.repo.clone(), git, Vec::new())
    }

    /// Starts the server on `repo`, with `git` isolated as the tests' own commands are.
    fn spawn(repo: PathBuf, git: &Isolation, mut dirs: Vec<TempDir>) -> Self {
        let tokens = TempDir::new().unwrap();
        let hashes: serde_json::Map<String, Value> = MACHINES
            .iter()
            .map(|(machine, token)| ((*machine).to_owned(), mneme_hash(token).into()))
            .collect();
        let file = tokens.path().join("tokens.json");
        fs::write(&file, Value::Object(hashes).to_string()).unwrap();
        dirs.push(tokens);
        let mut command = Command::new(env!("CARGO_BIN_EXE_mneme"));
        command
            .args(["serve", "--addr", "127.0.0.1:0", "--tokens"])
            .arg(&file)
            .arg("--repo")
            .arg(&repo)
            .stderr(Stdio::piped());
        let mut child = git.apply(&mut command).spawn().unwrap();
        let stderr = BufReader::new(child.stderr.take().unwrap());
        let log = Arc::new(Mutex::new(Vec::new()));
        let (ready, listening) = mpsc::channel();
        let lines = Arc::clone(&log);
        let reader = thread::spawn(move || {
            for line in stderr.lines().map_while(Result::ok) {
                if let Ok(event) = serde_json::from_str::<Value>(&line)
                    && event["event"] == "listening"
                {
                    let _ = ready.send(event["addr"].as_str().unwrap().to_owned());
                }
                lines.lock().unwrap().push(line);
            }
        });
        let addr = listening
            .recv_timeout(Duration::from_secs(60))
            .unwrap_or_else(|_| panic!("the server did not start: {:?}", log.lock().unwrap()));
        Self {
            child,
            url: format!("http://{addr}"),
            repo,
            log,
            reader: Some(reader),
            dirs,
        }
    }

    pub fn repo(&self) -> &Path {
        &self.repo
    }

    pub fn git_url(&self) -> String {
        format!("{}/git/wiki.git", self.url)
    }

    pub fn post(&self, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Reply {
        self.request("POST", path, headers, body)
    }

    /// One request on a connection of its own, which the server closes after the response.
    fn request(&self, method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Reply {
        let addr = self.url.trim_start_matches("http://");
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\
             Content-Length: {}\r\n",
            body.len()
        );
        for (name, value) in headers {
            let _ = write!(head, "{name}: {value}\r\n");
        }
        head.push_str("\r\n");
        let mut stream = TcpStream::connect(addr).unwrap();
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).unwrap();
        Reply::parse(&raw)
    }

    /// Sends SIGTERM, waits for the server to exit, and returns its status and whole log.
    pub fn terminate(mut self) -> (ExitStatus, Vec<String>) {
        self.stop()
    }

    /// Stops the server with SIGTERM and starts it again on the same repository. Returns the
    /// stopped server's status and log.
    pub fn restart(mut self, git: &Isolation) -> (ExitStatus, Vec<String>, Self) {
        let (status, log) = self.stop();
        let again = Self::spawn(self.repo.clone(), git, std::mem::take(&mut self.dirs));
        (status, log, again)
    }

    fn stop(&mut self) -> (ExitStatus, Vec<String>) {
        let pid = self.child.id().to_string();
        let sent = Command::new("kill").args(["-TERM", &pid]).status().unwrap();
        assert!(sent.success());
        let status = self.child.wait().unwrap();
        self.reader.take().unwrap().join().unwrap();
        (status, self.log())
    }

    pub fn get(&self, path: &str) -> Reply {
        self.get_with(path, &[])
    }

    pub fn get_with(&self, path: &str, headers: &[(&str, &str)]) -> Reply {
        self.request("GET", path, headers, b"")
    }

    /// A 200 answer as JSON.
    pub fn json(&self, path: &str) -> Value {
        let reply = self.get(path);
        assert_eq!(reply.status, 200, "{path}: {}", reply.body);
        reply.json()
    }

    /// Every line the server has logged so far.
    pub fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    /// The log once a line holds `needle`. The server writes a line before it answers, but the
    /// thread that reads its standard error can still be behind.
    pub fn log_with(&self, needle: &str) -> Vec<String> {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let log = self.log();
            if log.iter().any(|line| line.contains(needle)) {
                return log;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "no {needle:?} in {log:?}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}

fn mneme_hash(token: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .concat()
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
