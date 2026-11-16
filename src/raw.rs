//! `GET /v1/raw/{path}`: the bytes of one file, a page included, as they are on `main` or as
//! they were at a commit. The body is read from the repository as it is sent, so a file of any
//! size costs the same memory, and the route stays outside the reading API's `ETag` layer, which
//! reads a whole body to hash it. Nothing it sends can run as a document in a browser.

use std::io::{self, Read};
use std::sync::Arc;

use axum::body::{Body, Bytes};
use axum::extract::{Path, State};
use axum::http::header::{
    ACCEPT_RANGES, CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE,
    CONTENT_SECURITY_POLICY, CONTENT_TYPE, ETAG, IF_NONE_MATCH, IF_RANGE, RANGE,
    X_CONTENT_TYPE_OPTIONS,
};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use gix::ObjectId;
use serde::Deserialize;
use serde_json::json;

use crate::api::{ApiError, Params, matches};
use crate::blob::{self, TooLargeWhole};
use crate::index::Missing;
use crate::repo::Repo;
use crate::serve::Wiki;
use crate::{log, media};

/// How much of a file is read and sent at a time, which is all of it that is in memory at once.
const PIECE: usize = 64 << 10;
/// What a browser is told about every answer that is not a file, an error included.
const NOTHING: &str = "default-src 'none'; sandbox";

/// How a server sends a file's bytes.
#[derive(Clone, Copy, Debug)]
pub struct Sending {
    /// Whether the reading API asks for a token, so that no shared cache may keep an answer.
    pub private: bool,
    /// The largest blob read into memory whole when it cannot be read as a stream, such as one
    /// stored as a delta. `None` reads any blob whole.
    pub whole_up_to: Option<u64>,
}

#[derive(Clone)]
struct Raw {
    wiki: Arc<Wiki>,
    sending: Sending,
}

#[derive(Deserialize)]
struct At {
    at: Option<String>,
}

/// The route for a file's bytes.
pub fn router(wiki: Arc<Wiki>, sending: Sending) -> Router {
    Router::new()
        .route("/v1/raw/{*path}", get(raw))
        .with_state(Raw { wiki, sending })
}

/// The part of a file a request asks for.
#[derive(Debug, PartialEq, Eq)]
enum Part {
    Whole,
    /// From the first byte to the last, both counted from 0.
    Bytes(u64, u64),
    /// No byte of the file is in the range.
    Unmet,
}

async fn raw(
    State(raw): State<Raw>,
    method: Method,
    Path(path): Path<String>,
    Params(At { at }): Params<At>,
    headers: HeaderMap,
) -> Response {
    let mut response = answer(&raw, &method, path, at, &headers)
        .await
        .unwrap_or_else(IntoResponse::into_response);
    let headers = response.headers_mut();
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    headers
        .entry(CONTENT_SECURITY_POLICY)
        .or_insert(HeaderValue::from_static(NOTHING));
    response
}

async fn answer(
    raw: &Raw,
    method: &Method,
    path: String,
    at: Option<String>,
    asked: &HeaderMap,
) -> Result<Response, ApiError> {
    let at = at
        .map(|at| ObjectId::from_hex(at.as_bytes()))
        .transpose()
        .map_err(|_| ApiError::BadRequest("at is not a full commit id".to_owned()))?;
    let media = media::of(&path);
    let disposition = media::disposition(media, &path);
    let named = path.clone();
    let (blob, size) = raw
        .wiki
        .read(move |repo, index| {
            let (commit, missing) = match at {
                Some(at) => {
                    index.on_main(&at.to_string())?;
                    (at, Missing::BytesThen)
                }
                None => (index.head_id().ok_or(Missing::Bytes)?, Missing::Bytes),
            };
            let blob = repo.file_at(commit, &path)?.ok_or(missing)?;
            Ok((blob, repo.blob_size(blob)?))
        })
        .await?;

    // The blob's id names the bytes, so it is a strong validator.
    let tag = header(&format!("\"{blob}\""))?;
    let cache = match (raw.sending.private, at.is_some()) {
        (false, true) => "max-age=31536000, immutable",
        (false, false) => "no-cache",
        (true, true) => "private, max-age=31536000, immutable",
        (true, false) => "private, no-cache",
    };
    let mut headers = HeaderMap::new();
    headers.insert(ETAG, tag.clone());
    headers.insert(CACHE_CONTROL, HeaderValue::from_static(cache));
    // A cache takes the headers of a 304 into the answer it keeps, so the file's own policy goes
    // with it too, and not the one for answers that are not a file.
    headers.insert(
        CONTENT_SECURITY_POLICY,
        HeaderValue::from_static(media.policy),
    );
    if asked
        .get(IF_NONE_MATCH)
        .is_some_and(|known| matches(known, &tag))
    {
        return Ok((StatusCode::NOT_MODIFIED, headers).into_response());
    }
    headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    // A range is for the bytes the client already has part of: `If-Range` with another tag
    // means it has other bytes, and gets the whole file.
    let current = asked.get(IF_RANGE).is_none_or(|known| known == tag);
    let range = asked
        .get(RANGE)
        .filter(|_| current && method == Method::GET)
        .and_then(|range| range.to_str().ok());
    let (status, start, length) = match range.map_or(Part::Whole, |range| part(range, size)) {
        Part::Whole => (StatusCode::OK, 0, size),
        Part::Bytes(first, last) => {
            headers.insert(
                CONTENT_RANGE,
                header(&format!("bytes {first}-{last}/{size}"))?,
            );
            (StatusCode::PARTIAL_CONTENT, first, last - first + 1)
        }
        Part::Unmet => {
            headers.insert(CONTENT_RANGE, header(&format!("bytes */{size}"))?);
            let error = Json(json!({ "error": "the file has no byte in this range" }));
            return Ok((StatusCode::RANGE_NOT_SATISFIABLE, headers, error).into_response());
        }
    };
    headers.insert(CONTENT_TYPE, HeaderValue::from_static(media.content_type));
    headers.insert(CONTENT_DISPOSITION, header(&disposition)?);
    headers.insert(CONTENT_LENGTH, HeaderValue::from(length));
    let body = if method == Method::HEAD || length == 0 {
        Body::empty()
    } else {
        match open(&raw.wiki, blob, start, raw.sending.whole_up_to).await? {
            Ok(reader) => stream(reader, length),
            Err(too_large) => return Ok(not_streamable(&too_large, &named)),
        }
    };
    Ok((status, headers, body).into_response())
}

/// The answer for a blob the server will not read whole: 503, since the server cannot send
/// it as it stores it now, and can once the repository is repacked with its own settings. The
/// log names the blob and the path.
fn not_streamable(too_large: &TooLargeWhole, path: &str) -> Response {
    log::emit(
        "error",
        json!({
            "error": too_large.to_string(),
            "blob": too_large.blob.to_string(),
            "path": path,
        }),
    );
    let error = format!(
        "the file is stored as a delta of another and is over {} MiB, which the server does not \
         hold in memory; git repack -a -d -F in the server's repository stores it whole",
        too_large.most >> 20
    );
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "error": error })),
    )
        .into_response()
}

fn header(value: &str) -> Result<HeaderValue, ApiError> {
    HeaderValue::from_str(value).map_err(|error| ApiError::Internal(error.into()))
}

/// Opens `blob` and reads it up to `start`, so that a blob that cannot be read is an error and
/// not a body cut short. The inner error is a blob the server will not read whole.
async fn open(
    wiki: &Arc<Wiki>,
    blob: ObjectId,
    start: u64,
    whole_up_to: Option<u64>,
) -> Result<Result<blob::Reader, TooLargeWhole>, ApiError> {
    let opening = Arc::clone(wiki);
    let opened = tokio::task::spawn_blocking(move || -> anyhow::Result<blob::Reader> {
        let mut reader = Repo::open(opening.path())?.open_blob(blob, whole_up_to)?;
        reader.skip(start)?;
        Ok(reader)
    })
    .await
    .map_err(|error| ApiError::Internal(error.into()))?;
    match opened {
        Ok(reader) => Ok(Ok(reader)),
        Err(error) => match error.downcast::<TooLargeWhole>() {
            Ok(too_large) => Ok(Err(too_large)),
            Err(error) => Err(ApiError::Internal(error)),
        },
    }
}

/// `length` bytes from `reader`. Each piece is read when the client is ready for it, on a
/// thread where blocking is fine, so a slow client holds one piece in memory and no thread.
fn stream(reader: blob::Reader, length: u64) -> Body {
    let pieces =
        futures_util::stream::try_unfold((reader, length), |(mut reader, left)| async move {
            if left == 0 {
                return Ok(None);
            }
            let read = tokio::task::spawn_blocking(move || {
                let mut piece =
                    vec![0; usize::try_from(left).map_or(PIECE, |left| left.min(PIECE))];
                let read = reader.read(&mut piece)?;
                piece.truncate(read);
                io::Result::Ok((piece, reader))
            });
            let (piece, reader) = read.await.map_err(io::Error::other)??;
            if piece.is_empty() {
                return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
            }
            let left = left - piece.len() as u64;
            Ok(Some((Bytes::from(piece), (reader, left))))
        });
    Body::from_stream(pieces)
}

/// The part of a file of `size` bytes that a `Range` header asks for. One range of bytes is
/// met. Anything else, several ranges and a range that cannot be read among them, is the whole
/// file, as RFC 9110 lets a server answer.
fn part(range: &str, size: u64) -> Part {
    let Some((first, last)) = range
        .strip_prefix("bytes=")
        .filter(|range| !range.contains(','))
        .and_then(|range| range.trim().split_once('-'))
    else {
        return Part::Whole;
    };
    let last = match last {
        "" => None,
        last => match last.parse::<u64>() {
            Ok(last) => Some(last),
            Err(_) => return Part::Whole,
        },
    };
    if first.is_empty() {
        // The last so many bytes.
        return match last {
            None => Part::Whole,
            Some(0) => Part::Unmet,
            Some(_) if size == 0 => Part::Unmet,
            Some(count) => Part::Bytes(size.saturating_sub(count), size - 1),
        };
    }
    let Ok(first) = first.parse::<u64>() else {
        return Part::Whole;
    };
    match last {
        Some(last) if last < first => Part::Whole,
        _ if first >= size => Part::Unmet,
        Some(last) => Part::Bytes(first, last.min(size - 1)),
        None => Part::Bytes(first, size - 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_range_of_bytes_is_met_and_anything_else_is_the_whole_file() {
        for (range, size, expected) in [
            ("bytes=0-0", 10, Part::Bytes(0, 0)),
            ("bytes=2-5", 10, Part::Bytes(2, 5)),
            ("bytes=2-", 10, Part::Bytes(2, 9)),
            ("bytes=2-99", 10, Part::Bytes(2, 9)),
            ("bytes=-3", 10, Part::Bytes(7, 9)),
            ("bytes=-99", 10, Part::Bytes(0, 9)),
            ("bytes=9-9", 10, Part::Bytes(9, 9)),
            ("bytes=10-", 10, Part::Unmet),
            ("bytes=10-12", 10, Part::Unmet),
            ("bytes=-0", 10, Part::Unmet),
            ("bytes=0-", 0, Part::Unmet),
            ("bytes=-5", 0, Part::Unmet),
            ("bytes=0-1,4-5", 10, Part::Whole),
            ("bytes=5-2", 10, Part::Whole),
            ("bytes=12-10", 10, Part::Whole),
            ("bytes=a-b", 10, Part::Whole),
            ("bytes=-", 10, Part::Whole),
            ("bytes=", 10, Part::Whole),
            ("lines=0-1", 10, Part::Whole),
            ("0-1", 10, Part::Whole),
        ] {
            assert_eq!(part(range, size), expected, "{range} of {size}");
        }
    }
}
