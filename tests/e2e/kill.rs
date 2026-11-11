//! The real `mneme autosave run`, killed with SIGKILL at random moments and started again,
//! while the test writes pages in two folders. `MNEME_KILL_SEED` picks the moments.

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use serde_json::Value;
use tempfile::TempDir;

use crate::git::Isolation;
use crate::server::{MACHINES, Server};

/// A running `mneme autosave run` and what it has logged.
struct Service {
    child: Child,
    log: Arc<Mutex<Vec<String>>>,
    reader: Option<JoinHandle<()>>,
}

impl Service {
    fn start(git: &Isolation, args: &[String]) -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mneme"));
        command
            .args(["autosave", "run"])
            .args(args)
            .args([
                "--tick-seconds",
                "0.05",
                "--quiet-seconds",
                "0.2",
                "--fetch-seconds",
                "0.3",
            ])
            .stderr(Stdio::piped());
        let mut child = git.apply(&mut command).spawn().unwrap();
        let stderr = BufReader::new(child.stderr.take().unwrap());
        let log = Arc::new(Mutex::new(Vec::new()));
        let lines = Arc::clone(&log);
        let reader = thread::spawn(move || {
            for line in stderr.lines().map_while(Result::ok) {
                lines.lock().unwrap().push(line);
            }
        });
        Self {
            child,
            log,
            reader: Some(reader),
        }
    }

    /// SIGKILL, as a crash or `kill -9` would. Git commands it started run on to their end.
    fn kill(mut self) -> Vec<String> {
        self.child.kill().unwrap();
        self.child.wait().unwrap();
        self.reader.take().unwrap().join().unwrap();
        self.log.lock().unwrap().clone()
    }

    fn terminate(mut self) -> (ExitStatus, Vec<String>) {
        let pid = self.child.id().to_string();
        let sent = Command::new("kill").args(["-TERM", &pid]).status().unwrap();
        assert!(sent.success());
        let status = self.child.wait().unwrap();
        self.reader.take().unwrap().join().unwrap();
        (status, self.log.lock().unwrap().clone())
    }
}

/// The pages under `dir`, by path, with their text.
fn pages(dir: &Path) -> BTreeMap<String, String> {
    fn walk(dir: &Path, prefix: &str, pages: &mut BTreeMap<String, String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().into_string().unwrap();
            let path = format!("{prefix}{name}");
            if name == ".git" {
                continue;
            }
            if entry.file_type().unwrap().is_dir() {
                walk(&entry.path(), &format!("{path}/"), pages);
            } else {
                pages.insert(path, fs::read_to_string(entry.path()).unwrap());
            }
        }
    }
    let mut found = BTreeMap::new();
    walk(dir, "", &mut found);
    found
}

/// The pages of the server's `head`, read from that one commit, since `main` can move between
/// two reads while the services push.
fn server_pages(git: &Isolation, server: &Server, head: &str) -> Option<BTreeMap<String, String>> {
    let repo = server.repo();
    let listing = git.git(repo, &["ls-tree", "-r", "-z", "--name-only", head]);
    if !listing.status.success() {
        return None;
    }
    let names = String::from_utf8(listing.stdout).unwrap();
    Some(
        names
            .split('\0')
            .filter(|name| !name.is_empty())
            .map(|name| {
                let text = git.ok(repo, &["cat-file", "blob", &format!("{head}:{name}")]);
                (name.to_owned(), text)
            })
            .collect(),
    )
}

fn outcomes(log: &[String]) -> Vec<(String, String)> {
    log.iter()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|line| line["event"] == "autosave")
        .map(|line| {
            (
                line["outcome"].as_str().unwrap_or_default().to_owned(),
                line["detail"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

/// A folder `mneme autosave init` made for each machine, one missing and one empty, and the
/// arguments its service runs with.
fn init_folders(
    git: &Isolation,
    server: &Server,
    root: &Path,
    machines: &[(&str, &str)],
) -> Vec<(PathBuf, Vec<String>)> {
    let mut folders = Vec::new();
    for (index, (machine, token)) in machines.iter().enumerate() {
        let token_file = root.join(format!("{machine}.token"));
        fs::write(&token_file, format!("{token}\n")).unwrap();
        let folder = root.join(machine);
        if index == 1 {
            fs::create_dir(&folder).unwrap();
        }
        let args: Vec<String> = [
            "--folder",
            folder.to_str().unwrap(),
            "--server",
            &server.url,
            "--token-file",
            token_file.to_str().unwrap(),
            "--name",
            machine,
        ]
        .map(str::to_owned)
        .into();
        let init = git
            .apply(
                Command::new(env!("CARGO_BIN_EXE_mneme"))
                    .args(["autosave", "init"])
                    .args(&args),
            )
            .output()
            .unwrap();
        assert!(init.status.success(), "{init:?}");
        folders.push((folder, args));
    }
    folders
}

/// Leaves an `index.lock` an hour old in `folder`, as a Git command killed with its service
/// would have, unless a command that is still running holds the lock. Git takes the lock with
/// an exclusive create, and so does this.
fn leave_dead_lock(folder: &Path) -> bool {
    let lock = folder.join(".git/index.lock");
    let Ok(file) = fs::File::options().write(true).create_new(true).open(&lock) else {
        return false;
    };
    file.set_modified(SystemTime::now() - Duration::from_secs(3600))
        .unwrap();
    true
}

/// Waits until the server and every folder hold exactly `union`, all on one head.
fn wait_until_settled(
    git: &Isolation,
    server: &Server,
    folders: &[(PathBuf, Vec<String>)],
    union: &BTreeMap<String, String>,
    seed: u64,
) {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let head = git.git(server.repo(), &["rev-parse", "main"]);
        let head = String::from_utf8(head.stdout).unwrap();
        let settled = server_pages(git, server, head.trim()).as_ref() == Some(union)
            && folders.iter().all(|(folder, _)| {
                pages(folder) == *union
                    && git.ok(folder, &["rev-parse", "HEAD"]) == head
                    && git.ok(folder, &["status", "--porcelain"]).is_empty()
            });
        if settled {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "seed {seed}: the folders did not settle. Server: {:?}, expected {union:?}",
            server_pages(git, server, head.trim())
        );
        thread::sleep(Duration::from_millis(100));
    }
}

/// A second service on a folder that one already saves exits at once and says why, and the
/// first goes on.
#[test]
fn a_second_service_on_the_same_folder_exits() {
    let git = Isolation::new();
    let server = Server::empty(&git);
    let root = TempDir::new().unwrap();
    let folders = init_folders(&git, &server, root.path(), &[MACHINES[0]]);
    let (folder, args) = &folders[0];
    let first = Service::start(&git, args);
    let deadline = Instant::now() + Duration::from_secs(30);
    while outcomes(&first.log.lock().unwrap()).is_empty() {
        assert!(Instant::now() < deadline, "the first service ticks");
        thread::sleep(Duration::from_millis(20));
    }
    let mut second = Service::start(&git, args);
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        if let Some(status) = second.child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            let log = second.kill();
            panic!("the second service kept running: {log:?}");
        }
        thread::sleep(Duration::from_millis(20));
    };
    assert!(!status.success());
    second.reader.take().unwrap().join().unwrap();
    let said = second.log.lock().unwrap().join("\n");
    assert!(said.contains("already"), "{said}");
    assert!(said.contains(folder.to_str().unwrap()), "{said}");
    assert!(
        outcomes(&second.log.lock().unwrap()).is_empty(),
        "it never ticked"
    );

    fs::write(folder.join("after.md"), "# After\n").unwrap();
    let head = || {
        git.git(server.repo(), &["rev-parse", "-q", "--verify", "main"])
            .stdout
    };
    let before = head();
    let deadline = Instant::now() + Duration::from_secs(30);
    while head() == before {
        assert!(Instant::now() < deadline, "the first service still saves");
        thread::sleep(Duration::from_millis(50));
    }
    let (status, _) = first.terminate();
    assert!(status.success(), "{status}");
}

/// Each folder writes only its own pages, so in the end the server must hold exactly what the
/// test last wrote in both: nothing lost, and nothing deleted come back.
#[test]
fn services_killed_at_random_moments_lose_nothing() {
    let seed = std::env::var("MNEME_KILL_SEED").map_or(7, |seed| seed.parse().unwrap());
    let mut rng = fastrand::Rng::with_seed(seed);
    let git = Isolation::new();
    let server = Server::empty(&git);
    let root = TempDir::new().unwrap();
    let machines = [MACHINES[0], MACHINES[1]];
    let folders = init_folders(&git, &server, root.path(), &machines);
    let mut services: Vec<Option<Service>> = folders
        .iter()
        .map(|(_, args)| Some(Service::start(&git, args)))
        .collect();
    let mut logs: Vec<Vec<String>> = vec![Vec::new(); folders.len()];
    let mut expected: Vec<BTreeMap<String, String>> = vec![BTreeMap::new(); folders.len()];
    let mut kills = 0;
    let mut dead_locks = vec![0; folders.len()];
    for round in 0..80 {
        let at = rng.usize(..folders.len());
        let folder = &folders[at].0;
        let name = format!("{}-{}.md", machines[at].0, rng.usize(..12));
        if rng.u32(..5) == 0 && expected[at].contains_key(&name) {
            fs::remove_file(folder.join(&name)).unwrap();
            expected[at].remove(&name);
        } else {
            let text = format!("# {name}\n\nround {round}\n");
            fs::write(folder.join(&name), &text).unwrap();
            expected[at].insert(name, text);
        }
        thread::sleep(Duration::from_millis(rng.u64(..120)));
        if rng.u32(..3) == 0 {
            let victim = rng.usize(..folders.len());
            logs[victim].extend(services[victim].take().unwrap().kill());
            kills += 1;
            if rng.u32(..3) == 0 && leave_dead_lock(&folders[victim].0) {
                dead_locks[victim] += 1;
            }
            thread::sleep(Duration::from_millis(rng.u64(..60)));
            services[victim] = Some(Service::start(&git, &folders[victim].1));
        }
    }
    let union: BTreeMap<String, String> = expected.into_iter().flatten().collect();
    wait_until_settled(&git, &server, &folders, &union, seed);
    let merges = git.ok(server.repo(), &["rev-list", "--min-parents=2", "main"]);
    assert_eq!(merges, "", "seed {seed}: the server's history is one line");
    for (at, service) in services.into_iter().enumerate() {
        let (status, log) = service.unwrap().terminate();
        assert!(status.success(), "{status}");
        assert!(
            log.last().unwrap().contains(r#""event":"stopped""#),
            "{log:?}"
        );
        logs[at].extend(log);
    }
    for (at, (folder, _)) in folders.iter().enumerate() {
        let config = fs::read_to_string(folder.join(".git/config")).unwrap();
        let all = logs[at].join("\n");
        for (_, token) in machines {
            assert!(
                !config.contains(token),
                "the token is not in the repository's config"
            );
            assert!(!all.contains(token), "the token is never logged");
        }
        let changes = outcomes(&logs[at]);
        let stuck: Vec<_> = changes
            .iter()
            .filter(|(outcome, _)| outcome == "stopped" || outcome == "blocked")
            .collect();
        assert!(
            stuck.is_empty(),
            "seed {seed}: no tick was blocked or stopped: {stuck:?}"
        );
        let removed = logs[at]
            .iter()
            .filter(|line| line.contains(r#""event":"stale lock removed""#))
            .count();
        assert!(
            !folder.join(".git/index.lock").exists(),
            "seed {seed}: no lock is left"
        );
        assert!(
            removed <= dead_locks[at] && (dead_locks[at] == 0 || removed > 0),
            "seed {seed}: {} dead locks, {removed} removals logged",
            dead_locks[at]
        );
        eprintln!(
            "{}: {kills} kills in all, {} dead locks, {} outcome changes",
            machines[at].0,
            dead_locks[at],
            changes.len()
        );
    }
}
