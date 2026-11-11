//! The reading API: `GET` routes under `/v1` that answer with JSON.

use std::sync::Arc;

use axum::body::{Body, to_bytes};
use axum::extract::{FromRequestParts, Path, Query, Request, State};
use axum::http::header::{ETAG, IF_NONE_MATCH};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::index::{
    Filter, Hit, Listing, Missing, Order, Page, PageId, QueryError, Question, Sort, Status,
    Version, Window,
};
use crate::machines::Health;
use crate::serve::Wiki;
use crate::text::{self, DiffLine};
use crate::{index, log, now};

const DEFAULT_LIMIT: u32 = 50;
const MAX_LIMIT: u32 = 500;

/// The routes under `/v1`.
pub fn router(wiki: Arc<Wiki>) -> Router {
    Router::new()
        .route("/v1/status", get(status))
        .route("/v1/pages", get(pages))
        .route("/v1/pages/{id}", get(page))
        .route("/v1/pages/{id}/versions", get(versions))
        .route("/v1/pages/{id}/versions/{commit}", get(version))
        .route("/v1/pages/{id}/diff", get(diff))
        .route("/v1/changes", get(changes))
        .route("/v1/questions", get(questions))
        .route("/v1/search", get(search))
        .route("/v1/machines", get(machines))
        .fallback(|| async { ApiError::NotFound("no such route".to_owned()) })
        .layer(middleware::from_fn(etag))
        .with_state(wiki)
}

/// An error answered as `{ "error": "..." }`.
#[derive(Debug)]
pub enum ApiError {
    BadRequest(String),
    NotFound(String),
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for ApiError {
    fn from(error: anyhow::Error) -> Self {
        match error.downcast_ref::<Missing>() {
            Some(missing) => Self::NotFound(missing.to_string()),
            None => Self::Internal(error),
        }
    }
}

impl From<QueryError> for ApiError {
    fn from(error: QueryError) -> Self {
        Self::BadRequest(error.to_string())
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            Self::BadRequest(message) => (StatusCode::BAD_REQUEST, message),
            Self::NotFound(message) => (StatusCode::NOT_FOUND, message),
            Self::Internal(error) => {
                log::emit("error", json!({ "error": format!("{error:#}") }));
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error".to_owned(),
                )
            }
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}

/// Query parameters, with a JSON 400 when they do not parse.
struct Params<T>(T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequestParts<S> for Params<T> {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, ApiError> {
        let Query(params) = Query::from_request_parts(parts, state)
            .await
            .map_err(|rejection| ApiError::BadRequest(rejection.body_text()))?;
        Ok(Self(params))
    }
}

#[derive(Deserialize)]
struct ListParams {
    sort: Option<Sort>,
    order: Option<Order>,
    limit: Option<u32>,
    offset: Option<u32>,
    #[serde(default)]
    deleted: bool,
    changed_within_days: Option<u32>,
    created_within_days: Option<u32>,
    min_versions: Option<u32>,
    max_versions: Option<u32>,
    min_words: Option<u32>,
    max_words: Option<u32>,
}

impl ListParams {
    fn listing(self) -> Listing {
        Listing {
            sort: self.sort,
            order: self.order,
            limit: limit(self.limit),
            offset: self.offset.unwrap_or(0),
            deleted: self.deleted,
            filter: Filter {
                changed_within_days: self.changed_within_days,
                created_within_days: self.created_within_days,
                min_versions: self.min_versions,
                max_versions: self.max_versions,
                min_words: self.min_words,
                max_words: self.max_words,
            },
            now: now(),
        }
    }
}

#[derive(Deserialize)]
struct Paging {
    limit: Option<u32>,
    offset: Option<u32>,
    since: Option<i64>,
}

#[derive(Deserialize)]
struct Slice {
    limit: Option<u32>,
    offset: Option<u32>,
}

#[derive(Deserialize)]
struct SearchText {
    q: String,
}

#[derive(Deserialize)]
struct DiffParams {
    from: Option<String>,
    to: Option<String>,
}

#[derive(Serialize)]
struct PagesBody {
    total: u32,
    pages: Vec<Page>,
}

#[derive(Serialize)]
struct PageBody {
    page: Page,
    /// `None` for a deleted page.
    markdown: Option<String>,
    windows: Vec<Window>,
}

#[derive(Serialize)]
struct EventsBody {
    total: u32,
    events: Vec<Version>,
}

#[derive(Serialize)]
struct TextBody {
    page: PageId,
    commit: Option<String>,
    markdown: String,
}

#[derive(Serialize)]
struct DiffBody {
    from: Option<String>,
    to: Option<String>,
    /// `None` when the texts are too long to compare line by line.
    lines: Option<Vec<DiffLine>>,
}

#[derive(Serialize)]
struct QuestionsBody {
    total: u32,
    questions: Vec<Question>,
}

#[derive(Serialize)]
struct SearchBody {
    total: u32,
    hits: Vec<Hit>,
}

#[derive(Serialize)]
struct MachinesBody {
    total: usize,
    machines: Vec<Health>,
}

fn limit(limit: Option<u32>) -> u32 {
    limit.unwrap_or(DEFAULT_LIMIT).min(MAX_LIMIT)
}

async fn status(State(wiki): State<Arc<Wiki>>) -> Result<Json<Status>, ApiError> {
    wiki.read(|_, index| index.status()).await.map(Json)
}

async fn pages(
    State(wiki): State<Arc<Wiki>>,
    Params(params): Params<ListParams>,
) -> Result<Json<PagesBody>, ApiError> {
    let listing = params.listing();
    let (total, pages) = wiki.read(move |_, index| index.pages(&listing)).await?;
    Ok(Json(PagesBody { total, pages }))
}

async fn page(
    State(wiki): State<Arc<Wiki>>,
    Path(id): Path<String>,
) -> Result<Json<PageBody>, ApiError> {
    let id = PageId::from(id);
    wiki.read(move |repo, index| {
        let page = index.page(&id)?;
        let markdown = match index.text(&id, None)?.blob {
            Some(blob) => Some(repo.text(blob)?),
            None => None,
        };
        let windows = index.windows(&id, now())?;
        Ok(PageBody {
            page,
            markdown,
            windows,
        })
    })
    .await
    .map(Json)
}

async fn versions(
    State(wiki): State<Arc<Wiki>>,
    Path(id): Path<String>,
    Params(paging): Params<Paging>,
) -> Result<Json<EventsBody>, ApiError> {
    let id = PageId::from(id);
    let (total, events) = wiki
        .read(move |_, index| {
            index.page(&id)?;
            index.events(
                Some(&id),
                paging.since,
                limit(paging.limit),
                paging.offset.unwrap_or(0),
            )
        })
        .await?;
    Ok(Json(EventsBody { total, events }))
}

async fn version(
    State(wiki): State<Arc<Wiki>>,
    Path((id, commit)): Path<(String, String)>,
) -> Result<Json<TextBody>, ApiError> {
    let page = PageId::from(id);
    wiki.read(move |repo, index| {
        let end = index.text(&page, Some(&commit))?;
        let blob = end.blob.ok_or(Missing::Text)?;
        Ok(TextBody {
            markdown: repo.text(blob)?,
            commit: end.commit,
            page,
        })
    })
    .await
    .map(Json)
}

async fn diff(
    State(wiki): State<Arc<Wiki>>,
    Path(id): Path<String>,
    Params(params): Params<DiffParams>,
) -> Result<Json<DiffBody>, ApiError> {
    let page = PageId::from(id);
    wiki.read(move |repo, index| {
        let index::Diff { from, to } =
            index.diff(&page, params.from.as_deref(), params.to.as_deref())?;
        let text = |end: &index::End| match end.blob {
            Some(blob) => repo.text(blob),
            None => Ok(String::new()),
        };
        Ok(DiffBody {
            lines: text::diff(&text(&from)?, &text(&to)?),
            from: from.commit,
            to: to.commit,
        })
    })
    .await
    .map(Json)
}

async fn changes(
    State(wiki): State<Arc<Wiki>>,
    Params(paging): Params<Paging>,
) -> Result<Json<EventsBody>, ApiError> {
    let (total, events) = wiki
        .read(move |_, index| {
            index.events(
                None,
                paging.since,
                limit(paging.limit),
                paging.offset.unwrap_or(0),
            )
        })
        .await?;
    Ok(Json(EventsBody { total, events }))
}

async fn questions(
    State(wiki): State<Arc<Wiki>>,
    Params(slice): Params<Slice>,
) -> Result<Json<QuestionsBody>, ApiError> {
    let (total, questions) = wiki
        .read(move |_, index| index.questions(limit(slice.limit), slice.offset.unwrap_or(0)))
        .await?;
    Ok(Json(QuestionsBody { total, questions }))
}

async fn search(
    State(wiki): State<Arc<Wiki>>,
    Params(text): Params<SearchText>,
    Params(params): Params<ListParams>,
) -> Result<Json<SearchBody>, ApiError> {
    let query = index::Query::parse(&text.q)?;
    let listing = params.listing();
    let (total, hits) = wiki
        .read(move |_, index| index.search(&query, &listing))
        .await?;
    Ok(Json(SearchBody { total, hits }))
}

async fn machines(State(wiki): State<Arc<Wiki>>) -> Result<Json<MachinesBody>, ApiError> {
    let head = wiki.read(|_, index| Ok(index.status()?.head)).await?;
    let machines = wiki.machines().list(head.as_deref());
    Ok(Json(MachinesBody {
        total: machines.len(),
        machines,
    }))
}

/// A strong `ETag` from the hash of the body, and 304 when the client has it already.
async fn etag(request: Request, next: Next) -> Result<Response, ApiError> {
    let known = request.headers().get(IF_NONE_MATCH).cloned();
    let response = next.run(request).await;
    let (mut parts, body) = response.into_parts();
    let bytes = to_bytes(body, usize::MAX)
        .await
        .map_err(|error| ApiError::Internal(error.into()))?;
    let digest = Sha256::digest(&bytes);
    let tag = format!("\"{}\"", crate::hex(&digest[..16]));
    let tag = HeaderValue::from_str(&tag).map_err(|error| ApiError::Internal(error.into()))?;
    let fresh = parts.status.is_success() && known.is_some_and(|known| matches(&known, &tag));
    parts.headers.insert(ETAG, tag);
    if fresh {
        let mut headers = HeaderMap::new();
        headers.insert(ETAG, parts.headers[ETAG].clone());
        return Ok((StatusCode::NOT_MODIFIED, headers).into_response());
    }
    Ok(Response::from_parts(parts, Body::from(bytes)))
}

/// Whether an `If-None-Match` value names `tag`. The comparison is weak, as RFC 9110 asks.
fn matches(known: &HeaderValue, tag: &HeaderValue) -> bool {
    let Ok(known) = known.to_str() else {
        return false;
    };
    known.split(',').map(str::trim).any(|candidate| {
        candidate == "*" || candidate.trim_start_matches("W/").as_bytes() == tag.as_bytes()
    })
}
