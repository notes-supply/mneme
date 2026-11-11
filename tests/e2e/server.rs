//! The real `mneme serve` on a free port, and plain HTTP to it.

use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use serde_json::Value;
use tempfile::TempDir;
use ureq::Agent;

use crate::git::{Isolation, Work};

/// The token of the machine `blackbox`, the one machine the test servers know.
pub const TOKEN: &str = "test-token-of-blackbox";
pub const AUTHORIZATION: &str = "Authorization: Bearer test-token-of-blackbox";

pub struct Server {
    child: Child,
    pub url: String,
    repo: PathBuf,
    log: Arc<Mutex<Vec<String>>>,
    reader: Option<thread::JoinHandle<()>>,
    agent: Agent,
    _dirs: Vec<TempDir>,
}

pub struct Reply {
    pub status: u16,
    pub headers: ureq::http::HeaderMap,
    pub body: String,
}

impl Reply {
    pub fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or_else(|error| panic!("{error}: {}", self.body))
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|value| value.to_str().ok())
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
        let hash = mneme_hash(TOKEN);
        let file = tokens.path().join("tokens.json");
        fs::write(&file, format!(r#"{{ "blackbox": "{hash}" }}"#)).unwrap();
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
        let agent = Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        Self {
            child,
            url: format!("http://{addr}"),
            repo,
            log,
            reader: Some(reader),
            agent,
            _dirs: dirs,
        }
    }

    pub fn repo(&self) -> &Path {
        &self.repo
    }

    pub fn git_url(&self) -> String {
        format!("{}/git/wiki.git", self.url)
    }

    pub fn post(&self, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Reply {
        let mut request = self.agent.post(format!("{}{path}", self.url));
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        reply(request.send(body).unwrap())
    }

    /// Sends SIGTERM, waits for the server to exit, and returns its status and whole log.
    pub fn terminate(mut self) -> (ExitStatus, Vec<String>) {
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
        let mut request = self.agent.get(format!("{}{path}", self.url));
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        reply(request.call().unwrap())
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
}

fn mneme_hash(token: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(token.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .concat()
}

fn reply(mut response: ureq::http::Response<ureq::Body>) -> Reply {
    Reply {
        status: response.status().as_u16(),
        headers: response.headers().clone(),
        body: response.body_mut().read_to_string().unwrap(),
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
