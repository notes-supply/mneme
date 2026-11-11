//! `mneme serve`: the index, the reading API and the HTTP server around them.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use axum::Router;
use axum::extract::Request;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::get;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::oneshot;

use crate::api::{self, ApiError};
use crate::auth::Tokens;
use crate::git_http::{self, Machine};
use crate::git_version;
use crate::index::Index;
use crate::log;
use crate::machines::Machines;
use crate::repo::Repo;

/// The longest a change to the machines' state waits before it is written.
const SAVE_EVERY: Duration = Duration::from_secs(3);

/// Where `mneme serve` finds its repository and listens.
#[derive(Debug, clap::Args)]
pub struct Args {
    /// The bare repository of the wiki. It is created when it is missing.
    #[arg(long)]
    pub repo: PathBuf,
    /// A JSON object that maps each machine's name to the SHA-256 of its token.
    #[arg(long)]
    pub tokens: PathBuf,
    /// The address to listen on.
    #[arg(long, default_value = "0.0.0.0:8080")]
    pub addr: SocketAddr,
}

/// The repository, its index and the machines that write to it, shared by every request.
/// Each blocking task opens the repository itself, since a gix repository stays on one thread.
pub struct Wiki {
    path: PathBuf,
    index: Mutex<Index>,
    machines: Machines,
}

impl Wiki {
    /// The bare repository.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// What each machine last did and said.
    #[must_use]
    pub fn machines(&self) -> &Machines {
        &self.machines
    }

    /// Adds new commits on main to the index. Returns how many there were.
    ///
    /// # Errors
    ///
    /// Fails when the repository cannot be read.
    pub async fn update(self: &Arc<Self>) -> Result<usize> {
        let wiki = Arc::clone(self);
        tokio::task::spawn_blocking(move || {
            let mut index = wiki.index.lock().unwrap_or_else(PoisonError::into_inner);
            index.update(&Repo::open(&wiki.path)?)
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

/// Builds the index and serves until SIGTERM or Ctrl-C.
///
/// # Errors
///
/// Fails when `git` is too old, the repository cannot be read or the address cannot be bound.
pub async fn run(args: Args) -> Result<()> {
    git_version::check(Path::new("git"), "mneme serve")?;
    let mut terminate = signal(SignalKind::terminate()).context("watching for SIGTERM")?;
    let tokens = Tokens::load(&args.tokens)?;
    let names: Vec<String> = tokens.names().map(str::to_owned).collect();
    let path = args.repo;
    let wiki = tokio::task::spawn_blocking(move || -> Result<Wiki> {
        git_http::prepare(&path)?;
        let machines = Machines::load(
            names.iter().map(String::as_str),
            path.join("mneme").join("machines.json"),
        );
        let index = Index::build(&Repo::open(&path)?)?;
        log::emit("indexed", json!(index.status()?));
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
    let listener = TcpListener::bind(args.addr)
        .await
        .with_context(|| format!("binding {}", args.addr))?;
    log::emit("listening", json!({ "addr": listener.local_addr()? }));
    axum::serve(listener, router(wiki, tokens))
        .with_graceful_shutdown(async move {
            tokio::select! {
                _ = terminate.recv() => {}
                _ = tokio::signal::ctrl_c() => {}
            }
        })
        .await?;
    let _ = stop.send(());
    saving.await?;
    log::emit("stopped", json!({}));
    Ok(())
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

fn router(wiki: Arc<Wiki>, tokens: Tokens) -> Router {
    Router::new()
        .route("/healthz", get(|| async { "ok" }))
        .merge(api::router(Arc::clone(&wiki)))
        .merge(git_http::router(wiki, tokens))
        .layer(middleware::from_fn(request_log))
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
