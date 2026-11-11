//! `mneme serve`: the index, the reading API and the HTTP server around them.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use anyhow::{Context, Result};
use axum::Router;
use axum::extract::Request;
use axum::middleware::{self, Next};
use axum::response::Response;
use axum::routing::get;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};

use crate::api::{self, ApiError};
use crate::auth::Tokens;
use crate::git_http::{self, Machine};
use crate::index::Index;
use crate::log;
use crate::repo::Repo;

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

/// The repository and its index, shared by every request. Each blocking task opens the
/// repository itself, since a gix repository stays on one thread.
pub struct Wiki {
    path: PathBuf,
    index: Mutex<Index>,
}

impl Wiki {
    /// The bare repository.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
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
/// Fails when the repository cannot be read or the address cannot be bound.
pub async fn run(args: Args) -> Result<()> {
    let mut terminate = signal(SignalKind::terminate()).context("watching for SIGTERM")?;
    let tokens = Tokens::load(&args.tokens)?;
    let path = args.repo;
    let wiki = tokio::task::spawn_blocking(move || -> Result<Wiki> {
        git_http::prepare(&path)?;
        let index = Index::build(&Repo::open(&path)?)?;
        log::emit("indexed", json!(index.status()?));
        Ok(Wiki {
            path,
            index: Mutex::new(index),
        })
    })
    .await??;
    let wiki = Arc::new(wiki);
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
    log::emit("stopped", json!({}));
    Ok(())
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
