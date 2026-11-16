//! Git smart HTTP under `/git/wiki.git/`, by running `git http-backend` as a CGI child. Bodies
//! stream through in both directions, so a pack is never held in memory whole. A machine with a
//! read-only token may fetch and report, and is refused before Git runs when it would push.

use std::path::Path;
use std::process::{Command as StdCommand, Stdio};
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::header::{AUTHORIZATION, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::any;
use axum::{Json, Router};
use futures_util::StreamExt;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdout, Command};
use tokio::sync::mpsc;

use crate::auth::{Access, Machine, Tokens, unauthorized};
use crate::machines::{MAX_REPORT, REPORT_HEADER, Report, Unreadable};
use crate::serve::Wiki;
use crate::{hook, log, now, rules};

const PREFIX: &str = "/git/wiki.git";
/// The largest pack a push may carry: ten files of the largest size the rules let in, and the
/// pages around them. Git writes a pack to the repository's volume before the push check can
/// read it, so this is the most of the volume that one push can take before it is refused. Git
/// refuses a larger pack itself, as `receive.maxInputSize`, with a reason the pushing client
/// shows.
pub const MAX_PACK: u64 = 1 << 30;
/// The most of a request body that Git is given: a pack and the few lines before it. A body
/// with a longer declared length is refused before Git runs.
const MAX_BODY: u64 = MAX_PACK + (1 << 20);
const MAX_HEADERS: usize = 64 << 10;
/// How much of the child's standard error is logged. The rest is read and dropped.
const MAX_STDERR: u64 = 16 << 10;

/// The services of smart HTTP that `info/refs?service=` names.
const UPLOAD: &str = "git-upload-pack";
const RECEIVE: &str = "git-receive-pack";

#[derive(Clone)]
struct Git {
    wiki: Arc<Wiki>,
    tokens: Arc<Tokens>,
}

/// The smart HTTP routes. Every request needs a machine's token, and may carry its autosave
/// service's report.
pub fn router(wiki: Arc<Wiki>, tokens: Arc<Tokens>) -> Router {
    let git = Git { wiki, tokens };
    Router::new()
        .route(&format!("{PREFIX}/info/refs"), any(backend))
        .route(&format!("{PREFIX}/git-upload-pack"), any(backend))
        .route(&format!("{PREFIX}/git-receive-pack"), any(backend))
        .with_state(git)
}

/// Creates the bare repository when it is missing, sets the receive rules and installs the
/// push check. Running it again changes nothing.
///
/// # Errors
///
/// Fails when `git` fails or the hook cannot be written.
pub fn prepare(repo: &Path) -> Result<()> {
    if !repo.join("HEAD").exists() {
        let mut init = StdCommand::new("git");
        init.args(["init", "--quiet", "--bare", "--initial-branch=main"])
            .arg(repo);
        run(&mut init)?;
    }
    for (key, value) in [
        ("receive.denyNonFastForwards", "true".to_owned()),
        ("receive.denyDeletes", "true".to_owned()),
        ("receive.fsckObjects", "true".to_owned()),
        ("http.receivepack", "true".to_owned()),
        ("core.bigFileThreshold", rules::STREAMED_OVER.to_string()),
        ("receive.maxInputSize", MAX_PACK.to_string()),
    ] {
        let mut config = StdCommand::new("git");
        config
            .arg("--git-dir")
            .arg(repo)
            .args(["config", key, &value]);
        run(&mut config)?;
    }
    hook::install(repo)
}

fn run(command: &mut StdCommand) -> Result<()> {
    let output = command.output().context("running git")?;
    if !output.status.success() {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim());
    }
    Ok(())
}

/// The service a request runs, from its path and, for `info/refs`, the last `service` in its
/// query, as `git http-backend` reads it. Only the two services are named; anything else is the
/// ref listing of the dumb protocol, `None`.
fn service(path_info: &str, query: Option<&str>) -> Option<&'static str> {
    match path_info {
        "/git-upload-pack" => Some(UPLOAD),
        "/git-receive-pack" => Some(RECEIVE),
        _ => query?
            .split('&')
            .filter_map(|pair| pair.strip_prefix("service="))
            .next_back()
            .and_then(|named| [UPLOAD, RECEIVE].into_iter().find(|known| *known == named)),
    }
}

/// A report the request carries is stored, and the request is not counted as the machine's
/// last sync. A report that cannot be stored fails the request, so its service sends it again.
async fn backend(State(git): State<Git>, request: Request) -> Response {
    let Some((machine, access)) = git
        .tokens
        .machine(request.headers().get(AUTHORIZATION))
        .map(|(machine, access)| (machine.to_owned(), access))
    else {
        return unauthorized();
    };
    let path_info = request
        .uri()
        .path()
        .strip_prefix(PREFIX)
        .unwrap_or_default();
    if access == Access::Read && service(path_info, request.uri().query()) == Some(RECEIVE) {
        let reason =
            format!("mneme: {machine} has a read-only token, which may fetch but not push\n");
        let mut response = (
            StatusCode::FORBIDDEN,
            [(CONTENT_TYPE, "text/plain; charset=utf-8")],
            reason,
        )
            .into_response();
        response.extensions_mut().insert(Machine(machine));
        return response;
    }
    match request
        .headers()
        .get(REPORT_HEADER)
        .map(|value| Report::decode(value.as_bytes()))
    {
        None => git.wiki.machines().saw(&machine, now()),
        Some(Ok(report)) => git.wiki.machines().report(&machine, report, now()),
        Some(Err(unreadable)) => {
            let (status, error) = match unreadable {
                Unreadable::TooLarge => (
                    StatusCode::PAYLOAD_TOO_LARGE,
                    format!("the report is over {MAX_REPORT} bytes"),
                ),
                Unreadable::Garbled(error) => (StatusCode::BAD_REQUEST, error),
                Unreadable::Wrong(error) => (StatusCode::UNPROCESSABLE_ENTITY, error),
            };
            let mut response = (status, Json(json!({ "error": error }))).into_response();
            response.extensions_mut().insert(Machine(machine));
            return response;
        }
    }
    let too_large = request
        .headers()
        .get(CONTENT_LENGTH)
        .and_then(|length| length.to_str().ok()?.parse::<u64>().ok())
        .is_some_and(|length| length > MAX_BODY);
    let mut response = if too_large {
        StatusCode::PAYLOAD_TOO_LARGE.into_response()
    } else {
        cgi(git.wiki, &machine, request)
            .await
            .unwrap_or_else(|error| {
                log::emit("error", json!({ "error": format!("{error:#}") }));
                StatusCode::INTERNAL_SERVER_ERROR.into_response()
            })
    };
    response.extensions_mut().insert(Machine(machine));
    response
}

async fn cgi(wiki: Arc<Wiki>, machine: &str, request: Request) -> Result<Response> {
    let (parts, body) = request.into_parts();
    let path_info = parts.uri.path().strip_prefix(PREFIX).unwrap_or_default();
    // http-backend gets only the service read here, so what was checked is what Git runs.
    let query = match service(path_info, parts.uri.query()) {
        Some(service) if path_info == "/info/refs" => format!("service={service}"),
        _ => String::new(),
    };
    let push = path_info == "/git-receive-pack";
    let mut command = Command::new("git");
    command
        .arg("http-backend")
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_PROJECT_ROOT", wiki.path())
        .env("GIT_HTTP_EXPORT_ALL", "1")
        .env("REQUEST_METHOD", parts.method.as_str())
        .env("PATH_INFO", path_info)
        .env("QUERY_STRING", query)
        .env("REMOTE_USER", machine);
    for (header, variable) in [
        (CONTENT_TYPE, "CONTENT_TYPE"),
        (CONTENT_LENGTH, "CONTENT_LENGTH"),
        (CONTENT_ENCODING, "HTTP_CONTENT_ENCODING"),
        (HeaderName::from_static("git-protocol"), "HTTP_GIT_PROTOCOL"),
    ] {
        if let Some(value) = parts.headers.get(header).and_then(|v| v.to_str().ok()) {
            command.env(variable, value);
        }
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("starting git http-backend")?;
    let stdin = child.stdin.take().context("no stdin")?;
    let mut stdout = BufReader::new(child.stdout.take().context("no stdout")?);
    let mut stderr = child.stderr.take().context("no stderr")?;
    let feeding = tokio::spawn(feed(body, stdin, MAX_BODY));
    let errors = tokio::spawn(async move {
        let mut text = String::new();
        let _ = (&mut stderr)
            .take(MAX_STDERR)
            .read_to_string(&mut text)
            .await;
        let _ = tokio::io::copy(&mut stderr, &mut tokio::io::sink()).await;
        text
    });
    let (status, headers) = cgi_headers(&mut stdout).await?;
    let (sender, receiver) = mpsc::channel(16);
    let machine = machine.to_owned();
    tokio::spawn(async move {
        pump(stdout, &sender).await;
        let exit = child.wait().await.ok().and_then(|status| status.code());
        let body_error = feeding
            .await
            .ok()
            .and_then(|fed| fed.err().map(|error| format!("{error:#}")));
        let stderr = errors.await.unwrap_or_default();
        if !stderr.trim().is_empty() || body_error.is_some() {
            log::emit(
                "git",
                json!({ "machine": machine, "stderr": stderr.trim(), "body": body_error }),
            );
        }
        if push {
            let indexed = wiki.update().await;
            log::emit(
                "push",
                json!({
                    "machine": machine,
                    "exit": exit,
                    "commits": indexed.as_ref().ok(),
                    "error": indexed.err().map(|error| format!("{error:#}")),
                }),
            );
        }
    });
    let stream = futures_util::stream::unfold(receiver, |mut receiver| async move {
        receiver.recv().await.map(|chunk| (chunk, receiver))
    });
    let mut response = Response::new(Body::from_stream(stream));
    *response.status_mut() = status;
    *response.headers_mut() = headers;
    Ok(response)
}

/// Streams the request body to the child, up to `most` bytes. Closing stdin ends the body.
///
/// Once the child has stopped reading, as Git does when a pack is over `receive.maxInputSize`,
/// or the body has passed `most`, the rest of the body is still read, and dropped. The response
/// ends only when the body has, so the client sends its whole pack and then reads why it was
/// refused. Were the connection closed under it, Git would report a broken connection, which an
/// autosave service takes for a server it cannot reach and tries again for ever.
async fn feed(body: Body, stdin: impl AsyncWrite + Unpin, most: u64) -> Result<()> {
    let mut stream = body.into_data_stream();
    let mut stdin = Some(stdin);
    let mut total = 0;
    let mut stopped = Ok(());
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        total += chunk.len() as u64;
        let Some(open) = &mut stdin else {
            continue;
        };
        let fed = if total > most {
            Err(anyhow::anyhow!("the request body is over {most} bytes"))
        } else {
            open.write_all(&chunk).await.map_err(anyhow::Error::from)
        };
        if fed.is_err() {
            stopped = fed;
            stdin = None;
        }
    }
    stopped
}

/// The CGI response headers, up to the blank line.
async fn cgi_headers(stdout: &mut BufReader<ChildStdout>) -> Result<(StatusCode, HeaderMap)> {
    let mut status = StatusCode::OK;
    let mut headers = HeaderMap::new();
    let mut line = String::new();
    let mut read = 0;
    loop {
        line.clear();
        let length = stdout.read_line(&mut line).await?;
        read += length;
        if length == 0 || read > MAX_HEADERS {
            bail!("git http-backend sent no complete headers");
        }
        let line = line.trim_end_matches(['\r', '\n']);
        if line.is_empty() {
            return Ok((status, headers));
        }
        let (name, value) = line
            .split_once(':')
            .with_context(|| format!("unexpected CGI header {line:?}"))?;
        let value = value.trim();
        if name.eq_ignore_ascii_case("status") {
            let code = value.split(' ').next().unwrap_or_default();
            status = StatusCode::from_bytes(code.as_bytes())?;
        } else {
            headers.append(
                HeaderName::from_bytes(name.as_bytes())?,
                HeaderValue::from_str(value)?,
            );
        }
    }
}

/// Copies the child's output to the response. When the client has gone, the output is still
/// read to its end, so the child is never stuck on a full pipe.
async fn pump(mut stdout: BufReader<ChildStdout>, sender: &mpsc::Sender<std::io::Result<Bytes>>) {
    let mut buffer = vec![0; 64 << 10];
    let mut open = true;
    loop {
        match stdout.read(&mut buffer).await {
            Ok(0) => return,
            Ok(length) => {
                if open {
                    let chunk = Bytes::copy_from_slice(&buffer[..length]);
                    open = sender.send(Ok(chunk)).await.is_ok();
                }
            }
            Err(error) => {
                let _ = sender.send(Err(error)).await;
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context as TaskContext, Poll};

    use super::*;

    /// A child that has stopped reading its standard input.
    struct Closed;

    impl AsyncWrite for Closed {
        fn poll_write(
            self: Pin<&mut Self>,
            _: &mut TaskContext<'_>,
            _: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()))
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            _: &mut TaskContext<'_>,
        ) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }

    /// A body of `chunks` chunks of ten bytes, and how many of them were read.
    fn body(chunks: usize) -> (Body, Arc<AtomicUsize>) {
        let read = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&read);
        let stream = futures_util::stream::iter(0..chunks).map(move |_| {
            counted.fetch_add(1, Ordering::Relaxed);
            Ok::<_, std::io::Error>(Bytes::from_static(b"ten bytes!"))
        });
        (Body::from_stream(stream), read)
    }

    #[test]
    fn a_body_is_read_to_its_end_even_when_it_is_too_long_or_git_has_stopped_reading() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        runtime.block_on(async {
            let (whole, read) = body(5);
            let mut fed = Vec::new();
            feed(whole, &mut fed, 50).await.unwrap();
            assert_eq!((fed.len(), read.load(Ordering::Relaxed)), (50, 5));

            let (long, read) = body(5);
            let mut fed = Vec::new();
            let error = feed(long, &mut fed, 25).await.unwrap_err();
            assert_eq!(error.to_string(), "the request body is over 25 bytes");
            assert_eq!(fed.len(), 20, "only whole chunks under the limit reach Git");
            assert_eq!(
                read.load(Ordering::Relaxed),
                5,
                "the rest is read and dropped"
            );

            let (unread, read) = body(5);
            let error = feed(unread, Closed, 50).await.unwrap_err();
            assert!(error.to_string().contains("roken pipe"), "{error}");
            assert_eq!(
                read.load(Ordering::Relaxed),
                5,
                "the rest is read and dropped"
            );
        });
    }

    #[test]
    fn the_service_is_read_from_the_path_or_the_last_service_in_the_query() {
        for (path, query, service_named) in [
            ("/git-upload-pack", None, Some(UPLOAD)),
            (
                "/git-receive-pack",
                Some("service=git-upload-pack"),
                Some(RECEIVE),
            ),
            ("/info/refs", Some("service=git-upload-pack"), Some(UPLOAD)),
            (
                "/info/refs",
                Some("service=git-receive-pack"),
                Some(RECEIVE),
            ),
            (
                "/info/refs",
                Some("service=git-upload-pack&service=git-receive-pack"),
                Some(RECEIVE),
            ),
            (
                "/info/refs",
                Some("service=git-receive-pack&x=1"),
                Some(RECEIVE),
            ),
            ("/info/refs", Some("service=git%2Dreceive-pack"), None),
            ("/info/refs", Some("service=git-receive-packs"), None),
            ("/info/refs", None, None),
        ] {
            assert_eq!(service(path, query), service_named, "{path} {query:?}");
        }
    }
}
