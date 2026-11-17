//! The routes that manage machines while the server runs: list, add or replace, and remove the
//! machines added through the API. Each asks for an admin token, which only the tokens file
//! gives, and a machine added here is only ever read or write. The server only ever sees a
//! token's hash.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, put};
use axum::{Extension, Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::auth::{self, Granted, Listed, Machine, Need, Refused, Tokens};
use crate::edit::{Body, Param};
use crate::serve::Wiki;
use crate::{log, now};

#[derive(Clone)]
struct Admin {
    wiki: Arc<Wiki>,
    tokens: Arc<Tokens>,
    /// Held across a change to the tokens and the bookkeeping that follows it, so that two
    /// changes to one machine can't end with its token added and its record forgotten.
    serial: Arc<tokio::sync::Mutex<()>>,
}

/// The routes under `/v1/tokens`. They ask for an admin token whether or not the reading API
/// asks for a token.
pub fn router(wiki: Arc<Wiki>, tokens: Arc<Tokens>) -> Router {
    let admin = Admin {
        wiki,
        tokens: Arc::clone(&tokens),
        serial: Arc::default(),
    };
    Router::new()
        .route("/v1/tokens", get(list))
        .route("/v1/tokens/{name}", put(add).delete(remove))
        .layer(middleware::from_fn_with_state(
            (tokens, Need::Admin),
            auth::guard,
        ))
        .with_state(admin)
}

#[derive(Serialize)]
struct Listing {
    total: usize,
    tokens: Vec<Listed>,
}

/// `PUT /v1/tokens/{name}`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Adding {
    sha256: String,
    access: Granted,
}

async fn list(State(admin): State<Admin>) -> Json<Listing> {
    let tokens = admin.tokens.added();
    Json(Listing {
        total: tokens.len(),
        tokens,
    })
}

async fn add(
    State(admin): State<Admin>,
    Extension(Machine(by)): Extension<Machine>,
    Param(name): Param,
    Body(adding): Body<Adding>,
) -> Response {
    let _one = admin.serial.lock().await;
    let tokens = Arc::clone(&admin.tokens);
    let asked = name.clone();
    let added = tokio::task::spawn_blocking(move || {
        tokens.add(&asked, &adding.sha256, adding.access, now())
    })
    .await;
    match added.unwrap_or_else(|error| Err(Refused::Failed(error.into()))) {
        Ok((listed, replaced)) => {
            admin.wiki.machines().admit(&name);
            log::emit(
                "token added",
                json!({ "by": by, "machine": name, "access": listed.access, "replaced": replaced }),
            );
            let status = if replaced {
                StatusCode::OK
            } else {
                StatusCode::CREATED
            };
            (status, Json(listed)).into_response()
        }
        Err(refused) => refusal(refused),
    }
}

async fn remove(
    State(admin): State<Admin>,
    Extension(Machine(by)): Extension<Machine>,
    Param(name): Param,
) -> Response {
    let _one = admin.serial.lock().await;
    let tokens = Arc::clone(&admin.tokens);
    let asked = name.clone();
    let removed = tokio::task::spawn_blocking(move || tokens.remove(&asked)).await;
    match removed.unwrap_or_else(|error| Err(Refused::Failed(error.into()))) {
        Ok(()) => {
            admin.wiki.machines().forget(&name);
            log::emit("token removed", json!({ "by": by, "machine": name }));
            StatusCode::NO_CONTENT.into_response()
        }
        Err(refused) => refusal(refused),
    }
}

/// `{ "error" }` with the status that says why.
fn refusal(refused: Refused) -> Response {
    let (status, error) = match refused {
        Refused::Name(error) => (StatusCode::UNPROCESSABLE_ENTITY, error),
        Refused::Hash => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "sha256 is not 64 hexadecimal characters".to_owned(),
        ),
        Refused::Configured(name) => (
            StatusCode::CONFLICT,
            format!("{name} is in the tokens file, which only the operator changes"),
        ),
        Refused::Taken(name) => (
            StatusCode::CONFLICT,
            format!("another machine has the hash given for {name}"),
        ),
        Refused::NotAdded(name) => (
            StatusCode::NOT_FOUND,
            format!("no machine named {name} was added through the API"),
        ),
        Refused::Failed(error) => {
            log::emit(
                "error",
                json!({ "error": format!("changing the added machines: {error:#}") }),
            );
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error".to_owned(),
            )
        }
    };
    (status, Json(json!({ "error": error }))).into_response()
}
