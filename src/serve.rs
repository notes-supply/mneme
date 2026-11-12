//! `mneme serve`: the index, the reading API and the HTTP server around them. With `--repo` it
//! is the server that clones push to. With `--folder` it reads a clone on this machine and
//! serves the reading API alone, on loopback.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use axum::extract::Request;
use axum::http::StatusCode;
use axum::http::header::HOST;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::json;
use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::oneshot;

use crate::api::{self, ApiError};
use crate::auth::{self, Machine, Tokens};
use crate::git_version;
use crate::index::{Index, Status};
use crate::machines::Machines;
use crate::repo::{NotOnMain, Repo};
use crate::{edit, git_http};
use crate::{log, now};

/// The longest a change to the machines' state waits before it is written.
const SAVE_EVERY: Duration = Duration::from_secs(3);
/// How often a folder's `main` is read for a new tip.
const FOLLOW_EVERY: Duration = Duration::from_secs(2);
/// How often `--exit-with-parent` looks for a new parent.
const PARENT_EVERY: Duration = Duration::from_secs(1);
/// Where the server listens unless `--addr` says otherwise.
const REPO_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8080);
/// Where a folder's reading API listens unless `--addr` says otherwise.
const FOLDER_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8081);

/// Where `mneme serve` finds the wiki and listens.
#[derive(Debug, clap::Args)]
#[command(group(clap::ArgGroup::new("tokens_from").args(["tokens", "tokens_env"])))]
pub struct Args {
    /// The bare repository of the wiki. It is created when it is missing.
    #[arg(long, required_unless_present = "folder", requires = "tokens_from")]
    pub repo: Option<PathBuf>,
    /// A JSON file that maps each machine's name to the SHA-256 of its token, or to an object
    /// with that hash as `sha256` and `access` set to `read` or `write`.
    #[arg(long, requires = "repo")]
    pub tokens: Option<PathBuf>,
    /// The same JSON as --tokens, from this environment variable instead of a file, as a
    /// container platform gives a secret.
    #[arg(long, value_name = "NAME", requires = "repo")]
    pub tokens_env: Option<String>,
    /// Ask every /v1 route for a machine's token, read-only or not. Without it the reading API
    /// has no login, and the network must keep it private.
    #[arg(long, requires = "repo")]
    pub private_api: bool,
    /// A clone of the wiki on this machine, whose `main` is read and followed. Only the reading
    /// API is served, on loopback, and nothing is written to the folder.
    #[arg(long, conflicts_with_all = ["repo", "tokens", "tokens_env", "private_api"])]
    pub folder: Option<PathBuf>,
    /// The address to listen on. The default is 127.0.0.1:8080 with --repo, and 127.0.0.1:8081
    /// with --folder, which listens on a loopback address only.
    #[arg(long)]
    pub addr: Option<SocketAddr>,
    /// Stop when the process that started this one exits.
    #[arg(long, hide = true)]
    pub exit_with_parent: bool,
}

/// The repository, its index and the machines that write to it, shared by every request.
/// Each blocking task opens the repository itself, since a gix repository stays on one thread.
pub struct Wiki {
    path: PathBuf,
    index: Mutex<Index>,
    machines: Machines,
}

/// What following a folder's `main` found, with the index's status after it.
enum Followed {
    /// So many new commits on top of the indexed head.
    Added(usize, Status),
    /// `main` no longer descends from the indexed head, so the index was built again.
    Rebuilt(Status),
}

impl Wiki {
    /// The repository: the server's bare one, or a folder's `.git`.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// What each machine last did and said.
    #[must_use]
    pub fn machines(&self) -> &Machines {
        &self.machines
    }

    /// Adds new commits on main to the index, and compares every machine with the new head.
    /// Returns how many commits there were.
    ///
    /// # Errors
    ///
    /// Fails when the repository cannot be read.
    pub async fn update(self: &Arc<Self>) -> Result<usize> {
        let wiki = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let mut index = wiki.index.lock().unwrap_or_else(PoisonError::into_inner);
            let added = index.update(&Repo::open(&wiki.path)?)?;
            wiki.machines.moved(index.head(), now());
            Ok(added)
        })
        .await?
    }

    /// Reads the tip of `main` and brings the index to it: adds the new commits, or builds the
    /// index again when `main` no longer descends from the indexed head, as when someone reset
    /// it by hand. Returns `None` when `main` has not moved.
    async fn follow(self: &Arc<Self>) -> Result<Option<Followed>> {
        let wiki = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let repo = Repo::open(&wiki.path)?;
            let tip = repo.head()?.map(|tip| tip.to_string());
            {
                let mut index = wiki.index.lock().unwrap_or_else(PoisonError::into_inner);
                if index.head() == tip {
                    return Ok(None);
                }
                match index.update(&repo) {
                    Ok(added) => return Ok(Some(Followed::Added(added, index.status()?))),
                    Err(error) if error.downcast_ref::<NotOnMain>().is_some() => {}
                    Err(error) => return Err(error),
                }
            }
            let fresh = Index::build(&repo)?;
            let status = fresh.status()?;
            *wiki.index.lock().unwrap_or_else(PoisonError::into_inner) = fresh;
            Ok(Some(Followed::Rebuilt(status)))
        })
        .await?
    }

    /// Runs `read` with the index locked, on a thread where blocking is fine.
    ///
    /// # Errors
    ///
    /// The error of `read`, or [`ApiError::Internal`] when the task fails.
    pub async fn read<T: Send + 'static>(
        self: &Arc<Self>,
        read: impl FnOnce(&Repo, &Index) -> Result<T> + Send + 'static,
    ) -> Result<T, ApiError> {
        let wiki = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            // A panic inside an update rolls its transaction back, so the index is whole.
            let index = wiki.index.lock().unwrap_or_else(PoisonError::into_inner);
            read(&Repo::open(&wiki.path)?, &index)
        })
        .await
        .map_err(|error| ApiError::Internal(error.into()))?
        .map_err(ApiError::from)
    }
}

/// Builds the index and serves until SIGTERM or Ctrl-C, or until the parent exits when it was
/// asked to.
///
/// # Errors
///
/// Fails when `git` is too old for the server, the repository cannot be read, or the address
/// cannot be bound or is not loopback for a folder.
pub async fn run(args: Args) -> Result<()> {
    match (&args.folder, &args.repo) {
        (Some(folder), None) => serve_folder(folder, &args).await,
        (None, Some(repo)) => serve_repo(repo.clone(), &args).await,
        _ => bail!("mneme serve takes --repo with --tokens or --tokens-env, or --folder"),
    }
}

/// The server: the bare repository over HTTPS for the clones, and the reading API.
async fn serve_repo(path: PathBuf, args: &Args) -> Result<()> {
    git_version::check(Path::new("git"), "mneme serve")?;
    let stopping = stop_signal(args.exit_with_parent)?;
    let tokens = match (&args.tokens, &args.tokens_env) {
        (Some(file), None) => Tokens::load(file)?,
        (None, Some(name)) => Tokens::from_env(name)?,
        _ => bail!("mneme serve --repo takes one of --tokens and --tokens-env"),
    };
    let tokens = Arc::new(tokens);
    let names: Vec<String> = tokens.names().map(str::to_owned).collect();
    let wiki = tokio::task::spawn_blocking(move || -> Result<Wiki> {
        git_http::prepare(&path)?;
        let machines = Machines::load(
            names.iter().map(String::as_str),
            path.join("mneme").join("machines.json"),
        );
        let index = Index::build(&Repo::open(&path)?)?;
        log::emit("indexed", json!(index.status()?));
        machines.moved(index.head(), now());
        Ok(Wiki {
            path,
            index: Mutex::new(index),
            machines,
        })
    })
    .await??;
    let wiki = Arc::new(wiki);
    let (stop, stopped) = oneshot::channel();
    let saving = tokio::spawn(save_machines(Arc::clone(&wiki), stopped));
    let mut reading = api::router(Arc::clone(&wiki));
    if args.private_api {
        let tokens = Arc::clone(&tokens);
        reading = reading.layer(middleware::from_fn_with_state(tokens, auth::any_token));
    }
    let router = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(reading)
        .merge(edit::router(Arc::clone(&wiki), Arc::clone(&tokens)))
        .merge(git_http::router(wiki, tokens))
        .layer(middleware::from_fn(request_log));
    listen(args.addr.unwrap_or(REPO_ADDR), router, stopping).await?;
    let _ = stop.send(());
    saving.await?;
    log::emit("stopped", json!({}));
    Ok(())
}

/// The reading API for a clone on this machine. It reads `main` in the folder's `.git` and
/// follows it, writes nothing, needs no `git`, and answers only on loopback to a loopback name,
/// since it has no login.
async fn serve_folder(folder: &Path, args: &Args) -> Result<()> {
    let addr = args.addr.unwrap_or(FOLDER_ADDR);
    if !addr.ip().to_canonical().is_loopback() {
        bail!(
            "mneme serve --folder listens on a loopback address only, since the reading API has \
             no login, and {addr} is not one"
        );
    }
    let path = folder.join(".git");
    if !path.is_dir() {
        bail!(
            "{} is not a clone of the wiki: it has no .git",
            folder.display()
        );
    }
    let stopping = stop_signal(args.exit_with_parent)?;
    let wiki = tokio::task::spawn_blocking(move || -> Result<Wiki> {
        let index = Index::build(&Repo::open(&path)?)?;
        log::emit("indexed", json!(index.status()?));
        Ok(Wiki {
            path,
            index: Mutex::new(index),
            machines: Machines::none(),
        })
    })
    .await??;
    let wiki = Arc::new(wiki);
    let following = tokio::spawn(follow(Arc::clone(&wiki)));
    let router = Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(api::router(wiki))
        .layer(middleware::from_fn(loopback_host))
        .layer(middleware::from_fn(request_log));
    listen(addr, router, stopping).await?;
    following.abort();
    log::emit("stopped", json!({}));
    Ok(())
}

/// Resolves on SIGTERM or Ctrl-C, and when `with_parent` is set, once this process has a new
/// parent because the one that started it exited.
fn stop_signal(with_parent: bool) -> Result<impl Future<Output = ()>> {
    let mut terminate = signal(SignalKind::terminate()).context("watching for SIGTERM")?;
    let parent = std::os::unix::process::parent_id();
    Ok(async move {
        let orphaned = async {
            if !with_parent {
                return std::future::pending().await;
            }
            let mut every = tokio::time::interval(PARENT_EVERY);
            loop {
                every.tick().await;
                if std::os::unix::process::parent_id() != parent {
                    log::emit("parent exited", json!({ "parent": parent }));
                    return;
                }
            }
        };
        tokio::select! {
            _ = terminate.recv() => {}
            _ = tokio::signal::ctrl_c() => {}
            () = orphaned => {}
        }
    })
}

/// Binds `addr`, says where it listens, and serves `router` until `stopping` resolves.
async fn listen(
    addr: SocketAddr,
    router: Router,
    stopping: impl Future<Output = ()> + Send + 'static,
) -> Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    log::emit("listening", json!({ "addr": listener.local_addr()? }));
    axum::serve(listener, router)
        .with_graceful_shutdown(stopping)
        .await?;
    Ok(())
}

/// Brings the index to the folder's `main` every [`FOLLOW_EVERY`]. An error is logged once until
/// it changes or goes away, and the old index keeps answering meanwhile.
async fn follow(wiki: Arc<Wiki>) {
    let mut every = tokio::time::interval(FOLLOW_EVERY);
    every.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut failing: Option<String> = None;
    loop {
        every.tick().await;
        match wiki.follow().await {
            Ok(None) => failing = None,
            Ok(Some(Followed::Added(commits, status))) => {
                failing = None;
                log::emit("followed", json!({ "commits": commits, "status": status }));
            }
            Ok(Some(Followed::Rebuilt(status))) => {
                failing = None;
                log::emit("rebuilt", json!({ "status": status }));
            }
            Err(error) => {
                let error = format!("reading the folder's main: {error:#}");
                if failing.as_ref() != Some(&error) {
                    log::emit("error", json!({ "error": error }));
                    failing = Some(error);
                }
            }
        }
    }
}

/// Refuses a request that does not name a loopback host, with 421, so that a web page cannot
/// reach the reading API by pointing a name of its own at 127.0.0.1.
async fn loopback_host(request: Request, next: Next) -> Response {
    let named = request
        .headers()
        .get(HOST)
        .and_then(|host| host.to_str().ok())
        .is_some_and(loopback);
    if named && request.uri().host().is_none_or(loopback) {
        return next.run(request).await;
    }
    (
        StatusCode::MISDIRECTED_REQUEST,
        Json(json!({
            "error": "the reading API answers only to localhost, 127.0.0.1 or [::1] in Host"
        })),
    )
        .into_response()
}

/// Whether `host`, as a `Host` header gives it, is `localhost`, `127.0.0.1` or `[::1]`, with
/// any port.
fn loopback(host: &str) -> bool {
    let (name, port) = if host.starts_with('[') {
        match host.find(']') {
            Some(end) => host.split_at(end + 1),
            None => return false,
        }
    } else {
        host.find(':').map_or((host, ""), |at| host.split_at(at))
    };
    let port = port.is_empty()
        || port
            .strip_prefix(':')
            .is_some_and(|port| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()));
    port && (name.eq_ignore_ascii_case("localhost") || name == "127.0.0.1" || name == "[::1]")
}

/// Writes the machines' state every [`SAVE_EVERY`] when it changed, and once more when `stop`
/// fires. It is the only writer of the state file.
async fn save_machines(wiki: Arc<Wiki>, mut stop: oneshot::Receiver<()>) {
    let mut every = tokio::time::interval(SAVE_EVERY);
    loop {
        let last = tokio::select! {
            _ = every.tick() => false,
            _ = &mut stop => true,
        };
        let saving = Arc::clone(&wiki);
        let saved = tokio::task::spawn_blocking(move || saving.machines().save()).await;
        if let Some(error) = saved
            .map_err(anyhow::Error::from)
            .and_then(|saved| saved)
            .err()
        {
            log::emit(
                "error",
                json!({ "error": format!("saving the machines: {error:#}") }),
            );
        }
        if last {
            return;
        }
    }
}

/// One line per request. The query string is left out, since it can hold search words.
async fn request_log(request: Request, next: Next) -> Response {
    let start = Instant::now();
    let method = request.method().to_string();
    let path = request.uri().path().to_owned();
    let response = next.run(request).await;
    let machine = response
        .extensions()
        .get::<Machine>()
        .map(|machine| machine.0.as_str());
    log::emit(
        "request",
        json!({
            "method": method,
            "path": path,
            "status": response.status().as_u16(),
            "ms": start.elapsed().as_millis(),
            "machine": machine,
        }),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_loopback_name_with_any_port_is_a_loopback_host() {
        for host in [
            "localhost",
            "LocalHost:8081",
            "127.0.0.1",
            "127.0.0.1:0",
            "[::1]",
            "[::1]:65535",
        ] {
            assert!(loopback(host), "{host}");
        }
        for host in [
            "",
            "example.com",
            "evil.example:8081",
            "localhost.",
            "localhost.example.com",
            "127.0.0.2",
            "127.0.0.1.nip.io",
            "0.0.0.0:8081",
            "[::2]",
            "[::1",
            "localhost:",
            "localhost:80a",
            "localhost:8081:1",
            "[::1]x",
        ] {
            assert!(!loopback(host), "{host}");
        }
    }
}
