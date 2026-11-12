//! The editing API of `mneme serve --repo`: routes that commit one change to `main`, for clients
//! that are not a folder of files, such as a web editor or an agent. Every route asks for a
//! token that may write.
//!
//! A change names its base, the commit of the newest event of the page it started from, and is
//! refused when the page has had another event since. Otherwise it is built on the current
//! `main` with Git's plumbing in an index of its own, never a work tree, and `main` moves only if
//! it is still the commit the change was built on. A push that lands meanwhile makes that fail,
//! and the change is checked and built again on top of it.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result, anyhow, bail};
use axum::extract::{DefaultBodyLimit, FromRequest, FromRequestParts, Request, State};
use axum::http::StatusCode;
use axum::http::header::{AUTHORIZATION, RETRY_AFTER};
use axum::http::request::Parts;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{post, put};
use axum::{Extension, Json, Router};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio::sync::Mutex;

use crate::api::ApiError;
use crate::auth::{Access, Machine, Tokens, unauthorized};
use crate::index::{Page, PageId};
use crate::repo::{EMPTY_TREE, MAIN, NO_OBJECT};
use crate::rules::{self, Entry};
use crate::serve::Wiki;
use crate::{log, now};

/// How many times a change is checked and built again when `main` moved under it, before it is
/// answered with 503.
const ATTEMPTS: usize = 4;
/// The largest request body. A page of 1 MiB can take six times that in JSON's escapes.
const MAX_BODY: usize = 8 << 20;
/// The longest commit message.
const MAX_MESSAGE: usize = 64 << 10;

/// The routes, and the lock that lets one change through at a time.
struct Editor {
    wiki: Arc<Wiki>,
    one_at_a_time: Mutex<()>,
}

/// The editing routes. They ask for a token that may write whether or not the reading API does.
pub fn router(wiki: Arc<Wiki>, tokens: Arc<Tokens>) -> Router {
    let editor = Arc::new(Editor {
        wiki,
        one_at_a_time: Mutex::new(()),
    });
    Router::new()
        .route("/v1/paths/{*path}", put(write).delete(remove))
        .route("/v1/moves", post(rename))
        .layer(DefaultBodyLimit::max(MAX_BODY))
        .layer(middleware::from_fn_with_state(tokens, write_token))
        .with_state(editor)
}

/// Lets a request through only with a token that may write, and gives the handler its machine.
/// A read-only token is 403, and no token or an unknown one 401 with no body.
async fn write_token(
    State(tokens): State<Arc<Tokens>>,
    mut request: Request,
    next: Next,
) -> Response {
    let (machine, access) = match tokens.machine(request.headers().get(AUTHORIZATION)) {
        Some((machine, access)) => (Machine(machine.to_owned()), access),
        None => return unauthorized(),
    };
    let mut response = if access == Access::Write {
        request.extensions_mut().insert(machine.clone());
        next.run(request).await
    } else {
        let error = format!("{} has a read-only token, which may not edit", machine.0);
        (StatusCode::FORBIDDEN, Json(json!({ "error": error }))).into_response()
    };
    response.extensions_mut().insert(machine);
    response
}

/// Why a change was not made, answered as JSON.
#[derive(Debug)]
enum Refusal {
    /// A path or a body that did not parse, with the status axum gives it.
    Unreadable(StatusCode, String),
    /// The change would break a rule of the wiki, or the request has a bad value: 422.
    Rule(String),
    /// The page is not as the client saw it: 409, with the commit of its newest event now.
    Conflict {
        error: String,
        current: Option<String>,
    },
    /// `main` kept moving: 503.
    Busy,
    Internal(anyhow::Error),
}

impl From<anyhow::Error> for Refusal {
    fn from(error: anyhow::Error) -> Self {
        Self::Internal(error)
    }
}

impl From<ApiError> for Refusal {
    fn from(error: ApiError) -> Self {
        match error {
            ApiError::Internal(error) => Self::Internal(error),
            ApiError::BadRequest(error) | ApiError::NotFound(error) => {
                Self::Internal(anyhow!("reading the index: {error}"))
            }
        }
    }
}

impl IntoResponse for Refusal {
    fn into_response(self) -> Response {
        let (status, body) = match self {
            Self::Unreadable(status, error) => (status, json!({ "error": error })),
            Self::Rule(error) => (StatusCode::UNPROCESSABLE_ENTITY, json!({ "error": error })),
            Self::Conflict { error, current } => (
                StatusCode::CONFLICT,
                json!({ "error": error, "current": current }),
            ),
            Self::Busy => {
                let error = "main kept moving while the change was made; try it again";
                let body = Json(json!({ "error": error }));
                return (StatusCode::SERVICE_UNAVAILABLE, [(RETRY_AFTER, "1")], body)
                    .into_response();
            }
            Self::Internal(error) => {
                log::emit("error", json!({ "error": format!("{error:#}") }));
                let error = "internal error";
                (StatusCode::INTERNAL_SERVER_ERROR, json!({ "error": error }))
            }
        };
        (status, Json(body)).into_response()
    }
}

/// A JSON body, refused as `{ "error" }` with axum's status when it does not parse: 400 for bad
/// JSON, 415 without `Content-Type: application/json`, 413 when too long and 422 for a body of
/// the wrong shape.
struct Body<T>(T);

impl<S: Send + Sync, T: DeserializeOwned> FromRequest<S> for Body<T> {
    type Rejection = Refusal;

    async fn from_request(request: Request, state: &S) -> Result<Self, Refusal> {
        Json::<T>::from_request(request, state)
            .await
            .map(|Json(body)| Self(body))
            .map_err(|rejection| Refusal::Unreadable(rejection.status(), rejection.body_text()))
    }
}

/// The page's path from the URL, decoded, or 400 as JSON when it is not UTF-8.
struct PagePath(String);

impl<S: Send + Sync> FromRequestParts<S> for PagePath {
    type Rejection = Refusal;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Refusal> {
        axum::extract::Path::<String>::from_request_parts(parts, state)
            .await
            .map(|axum::extract::Path(path)| Self(path))
            .map_err(|rejection| Refusal::Unreadable(rejection.status(), rejection.body_text()))
    }
}

/// Who wrote a change. Both are one line, and Git would rewrite `<` and `>`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Author {
    name: String,
    email: String,
}

impl Author {
    fn check(&self) -> Result<(), Refusal> {
        for (what, value) in [("name", &self.name), ("email", &self.email)] {
            let problem = if value.trim().is_empty() {
                "is empty"
            } else if value.contains(['\n', '\r']) {
                "is more than one line"
            } else if value.contains(['<', '>', '\0']) {
                "holds <, > or NUL"
            } else {
                continue;
            };
            return Err(Refusal::Rule(format!("the author's {what} {problem}")));
        }
        Ok(())
    }
}

/// `PUT /v1/paths/{path}`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Writing {
    markdown: String,
    /// Present, and `null` for a new page.
    #[serde(deserialize_with = "Option::deserialize")]
    base: Option<String>,
    author: Author,
    #[serde(default)]
    message: Option<String>,
}

/// `DELETE /v1/paths/{path}`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Removal {
    base: String,
    author: Author,
    #[serde(default)]
    message: Option<String>,
}

/// `POST /v1/moves`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Moving {
    from: String,
    to: String,
    base: String,
    author: Author,
    #[serde(default)]
    message: Option<String>,
}

/// What a change answers: the commit to name as the next base, and the page as the index has
/// it after the change.
#[derive(Serialize)]
struct Changed {
    commit: String,
    page: Page,
}

#[derive(Serialize)]
struct Deleted {
    commit: String,
}

/// One change to the tree of `main`.
enum Change {
    Write { path: String, markdown: String },
    Delete { path: String },
    Move { from: String, to: String },
}

impl Change {
    /// The path whose page the base is about.
    fn path(&self) -> &str {
        match self {
            Self::Write { path, .. } | Self::Delete { path } | Self::Move { from: path, .. } => {
                path
            }
        }
    }

    /// Where the page is after the change, or `None` once it is deleted.
    fn after(&self) -> Option<&str> {
        match self {
            Self::Write { path, .. } => Some(path),
            Self::Delete { .. } => None,
            Self::Move { to, .. } => Some(to),
        }
    }
}

/// A change as a request asks for it, checked.
struct Asked {
    change: Change,
    base: Option<String>,
    author: Author,
    message: String,
}

/// What came of a change.
struct Made {
    /// The new commit, or, when the change left the tree as it was, the commit of the page's
    /// newest event, which stays its base.
    commit: String,
    /// The page after the change, `None` for a deletion.
    page: Option<Page>,
    /// Whether a new page was made.
    created: bool,
}

async fn write(
    State(editor): State<Arc<Editor>>,
    Extension(Machine(machine)): Extension<Machine>,
    PagePath(path): PagePath,
    Body(body): Body<Writing>,
) -> Result<Response, Refusal> {
    let size = u64::try_from(body.markdown.len()).unwrap_or(u64::MAX);
    check_page(&path, size)?;
    let verb = if body.base.is_some() {
        "Edit"
    } else {
        "Create"
    };
    let message = message(body.message, || format!("{verb} {path}"))?;
    let asked = Asked {
        change: Change::Write {
            path,
            markdown: body.markdown,
        },
        base: body.base,
        author: body.author,
        message,
    };
    let made = editor.make(&machine, asked).await?;
    let status = if made.created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    let page = made.page.context("the page is not in the index")?;
    let changed = Changed {
        commit: made.commit,
        page,
    };
    Ok((status, Json(changed)).into_response())
}

async fn remove(
    State(editor): State<Arc<Editor>>,
    Extension(Machine(machine)): Extension<Machine>,
    PagePath(path): PagePath,
    Body(body): Body<Removal>,
) -> Result<Json<Deleted>, Refusal> {
    check_page(&path, 0)?;
    let message = message(body.message, || format!("Delete {path}"))?;
    let asked = Asked {
        change: Change::Delete { path },
        base: Some(body.base),
        author: body.author,
        message,
    };
    let made = editor.make(&machine, asked).await?;
    Ok(Json(Deleted {
        commit: made.commit,
    }))
}

async fn rename(
    State(editor): State<Arc<Editor>>,
    Extension(Machine(machine)): Extension<Machine>,
    Body(body): Body<Moving>,
) -> Result<Json<Changed>, Refusal> {
    check_page(&body.from, 0)?;
    check_page(&body.to, 0)?;
    if body.from == body.to {
        return Err(Refusal::Rule(format!("from and to are both {}", body.to)));
    }
    let message = message(body.message, || {
        format!("Move {} to {}", body.from, body.to)
    })?;
    let asked = Asked {
        change: Change::Move {
            from: body.from,
            to: body.to,
        },
        base: Some(body.base),
        author: body.author,
        message,
    };
    let made = editor.make(&machine, asked).await?;
    let page = made.page.context("the page is not in the index")?;
    Ok(Json(Changed {
        commit: made.commit,
        page,
    }))
}

/// Refuses a path that cannot be a page's, and a page of `size` bytes that breaks a rule.
fn check_page(path: &str, size: u64) -> Result<(), Refusal> {
    if let Some(bad) = rules::page_path(path) {
        return Err(Refusal::Rule(format!(
            "{path:?} is not a page's path: {bad}"
        )));
    }
    if let Some(broken) = rules::check(path.as_bytes(), Entry::File(size)) {
        return Err(Refusal::Rule(format!("{broken} at {path}")));
    }
    Ok(())
}

/// The commit message: the one asked for, or `default` when it is missing or blank. It ends in a
/// newline, as `git commit` leaves one.
fn message(asked: Option<String>, default: impl FnOnce() -> String) -> Result<String, Refusal> {
    let mut message = asked
        .filter(|message| !message.trim().is_empty())
        .unwrap_or_else(default);
    if message.len() > MAX_MESSAGE {
        return Err(Refusal::Rule("the message is over 64 KiB".to_owned()));
    }
    if message.contains('\0') {
        return Err(Refusal::Rule("the message holds a NUL".to_owned()));
    }
    if !message.ends_with('\n') {
        message.push('\n');
    }
    Ok(message)
}

/// What the index says of the page a change is about, at the head the change is built on.
struct Seen {
    head: Option<String>,
    /// The live page at the change's path, and the commit of its newest event.
    at: Option<(PageId, String)>,
}

impl Editor {
    /// Makes `asked` on `main` as `machine`, one change at a time. Each attempt brings the index
    /// to `main`, checks the page against the base, builds the commit on the index's head and
    /// moves `main` only if it is still that head. When it is not, a push landed, and the next
    /// attempt starts over from the new `main`.
    async fn make(&self, machine: &str, asked: Asked) -> Result<Made, Refusal> {
        asked.author.check()?;
        if let Some(base) = asked.base.as_deref()
            && !crate::full_commit_id(base)
        {
            let error = format!("base {base:?} is not a full commit id");
            return Err(Refusal::Rule(error));
        }
        let _one = self.one_at_a_time.lock().await;
        let asked = Arc::new(asked);
        for attempt in 1..=ATTEMPTS {
            self.wiki.update().await?;
            let seen = self.look(&asked).await?;
            let building = Arc::clone(&asked);
            let repo = self.wiki.path().to_owned();
            let (on, who) = (seen.head.clone(), machine.to_owned());
            let (commit, moved) = tokio::task::spawn_blocking(move || {
                let git = Plumbing::new(&repo)?;
                let built = git.build(on.as_deref(), &building, &who)?;
                let moved = match &built {
                    Some(commit) => git.swap(commit, on.as_deref(), &who)?,
                    None => true,
                };
                Ok::<_, Refusal>((built, moved))
            })
            .await
            .map_err(|error| Refusal::Internal(error.into()))??;
            if !moved {
                continue;
            }
            let Some(commit) = commit else {
                let newest = seen.at_commit().or(seen.head);
                let page = match seen.at {
                    Some((id, _)) => Some(self.page(id).await?),
                    None => None,
                };
                return Ok(Made {
                    commit: newest.context("an empty wiki has no commit")?,
                    page,
                    created: false,
                });
            };
            let indexed = self.wiki.update().await?;
            log::emit(
                "edit",
                json!({
                    "machine": machine,
                    "commit": commit,
                    "attempts": attempt,
                    "commits": indexed,
                }),
            );
            let page = match asked.change.after() {
                Some(path) => Some(self.left_at(&commit, path).await?),
                None => None,
            };
            return Ok(Made {
                commit,
                page,
                created: asked.base.is_none(),
            });
        }
        Err(Refusal::Busy)
    }

    /// The index's head and the page at the change's path, once they are known to be as the
    /// client saw them: the page's newest event is the base, or there is no page and no base, and
    /// a move's `to` is free.
    async fn look(&self, asked: &Arc<Asked>) -> Result<Seen, Refusal> {
        let looking = Arc::clone(asked);
        let (seen, taken) = self
            .wiki
            .read(move |_, index| {
                let at = index.live(looking.change.path())?;
                let taken = match &looking.change {
                    Change::Move { to, .. } => index.live(to)?.is_some(),
                    Change::Write { .. } | Change::Delete { .. } => false,
                };
                Ok((
                    Seen {
                        head: index.head(),
                        at,
                    },
                    taken,
                ))
            })
            .await?;
        let current = seen.at_commit();
        let path = asked.change.path();
        let error = match (&current, &asked.base) {
            (Some(_), None) => format!("a page is already at {path}"),
            (None, Some(_)) => format!("no page is at {path} now"),
            (Some(current), Some(base)) if current != base => {
                format!("the page at {path} has changed since its base")
            }
            _ => match &asked.change {
                Change::Move { to, .. } if taken => format!("a page is already at {to}"),
                _ => return Ok(seen),
            },
        };
        Err(Refusal::Conflict { error, current })
    }

    /// The page that `commit` left at `path`, as the index has it now.
    async fn left_at(&self, commit: &str, path: &str) -> Result<Page, Refusal> {
        let (commit, path) = (commit.to_owned(), path.to_owned());
        let id = self
            .wiki
            .read(move |_, index| index.left_at(&commit, &path))
            .await?
            .context("the change is not in the index")?;
        self.page(id).await
    }

    async fn page(&self, id: PageId) -> Result<Page, Refusal> {
        Ok(self.wiki.read(move |_, index| index.page(&id)).await?)
    }
}

impl Seen {
    fn at_commit(&self) -> Option<String> {
        self.at.as_ref().map(|(_, commit)| commit.clone())
    }
}

/// Git's plumbing on the bare repository, with an index of its own in a file under
/// `mneme/`, since the server's root and temporary folder may be read-only. The file goes when
/// this does.
struct Plumbing<'a> {
    repo: &'a Path,
    index: PathBuf,
}

impl Drop for Plumbing<'_> {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.index);
    }
}

/// Numbers the index files of one server, so that no two share a name.
static INDEXES: AtomicU64 = AtomicU64::new(0);

impl<'a> Plumbing<'a> {
    fn new(repo: &'a Path) -> Result<Self> {
        let folder = repo.join("mneme");
        std::fs::create_dir_all(&folder)
            .with_context(|| format!("creating {}", folder.display()))?;
        let number = INDEXES.fetch_add(1, Ordering::Relaxed);
        let index = folder.join(format!("edit-{}-{number}.index", std::process::id()));
        Ok(Self { repo, index })
    }

    /// `git` on the repository and this index, cut off from every other configuration and
    /// variable, with paths read as they are written.
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new("git");
        command
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("GIT_DIR", self.repo)
            .env("GIT_INDEX_FILE", &self.index)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_LITERAL_PATHSPECS", "1")
            .env("LC_ALL", "C")
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    /// Runs `git` with `input` on standard input and `env` set. Returns whether it worked, its
    /// output and what it said on standard error, each without the space around it.
    fn run(
        &self,
        args: &[&str],
        input: &[u8],
        env: &[(&str, &str)],
    ) -> Result<(bool, String, String)> {
        use std::io::Write as _;
        let mut command = self.command(args);
        command.envs(env.iter().copied());
        let mut child = command
            .spawn()
            .with_context(|| format!("starting git {}", args.join(" ")))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(input)?;
        }
        let output = child.wait_with_output()?;
        let stdout = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        Ok((output.status.success(), stdout, stderr))
    }

    /// The output of a command that must work, without the space around it.
    fn line(&self, args: &[&str], input: &[u8], env: &[(&str, &str)]) -> Result<String> {
        let (worked, stdout, stderr) = self.run(args, input, env)?;
        if !worked {
            bail!("git {} failed: {stderr}", args.join(" "));
        }
        Ok(stdout)
    }

    /// The commit of `change` on `head`, as `machine`, or `None` when it leaves the tree as it
    /// was.
    fn build(
        &self,
        head: Option<&str>,
        asked: &Asked,
        machine: &str,
    ) -> Result<Option<String>, Refusal> {
        match head {
            Some(head) => self.line(&["read-tree", head], b"", &[])?,
            None => self.line(&["read-tree", "--empty"], b"", &[])?,
        };
        match &asked.change {
            Change::Write { path, markdown } => {
                let mode = self
                    .entry(path)?
                    .map_or("100644".to_owned(), |(mode, _)| mode);
                let blob =
                    self.line(&["hash-object", "-w", "--stdin"], markdown.as_bytes(), &[])?;
                self.add(&mode, &blob, path)?;
            }
            Change::Delete { path } => self.remove(path)?,
            Change::Move { from, to } => {
                let (mode, blob) = self
                    .entry(from)?
                    .with_context(|| format!("{from} is not in the tree"))?;
                self.remove(from)?;
                self.add(&mode, &blob, to)?;
            }
        }
        let tree = self.line(&["write-tree"], b"", &[])?;
        let before = match head {
            Some(head) => self.line(&["rev-parse", &format!("{head}^{{tree}}")], b"", &[])?,
            None => EMPTY_TREE.to_owned(),
        };
        if tree == before {
            return Ok(None);
        }
        let date = format!("@{} +0000", now());
        let email = format!("api@{machine}");
        let env = [
            ("GIT_AUTHOR_NAME", asked.author.name.as_str()),
            ("GIT_AUTHOR_EMAIL", asked.author.email.as_str()),
            ("GIT_AUTHOR_DATE", date.as_str()),
            ("GIT_COMMITTER_NAME", machine),
            ("GIT_COMMITTER_EMAIL", email.as_str()),
            ("GIT_COMMITTER_DATE", date.as_str()),
        ];
        let mut args = vec!["commit-tree", tree.as_str()];
        if let Some(head) = head {
            args.extend(["-p", head]);
        }
        Ok(Some(self.line(&args, asked.message.as_bytes(), &env)?))
    }

    /// The mode and blob of `path` in the index, if it is there.
    fn entry(&self, path: &str) -> Result<Option<(String, String)>> {
        let listing = self.line(&["ls-files", "--stage", "-z", "--", path], b"", &[])?;
        Ok(listing
            .split('\0')
            .filter_map(|entry| entry.split_once('\t'))
            .find(|(_, listed)| *listed == path)
            .and_then(|(facts, _)| {
                let mut facts = facts.split(' ');
                Some((facts.next()?.to_owned(), facts.next()?.to_owned()))
            }))
    }

    /// Puts `blob` at `path`. Git refuses what `receive.fsckObjects` would refuse in a push, such
    /// as a name that NTFS or HFS+ reads as `.git`, once both of its protections are on, and a
    /// path that a file or a folder is in the way of.
    fn add(&self, mode: &str, blob: &str, path: &str) -> Result<(), Refusal> {
        let entry = format!("{mode},{blob},{path}");
        let args = [
            "-c",
            "core.protectHFS=true",
            "-c",
            "core.protectNTFS=true",
            "update-index",
            "--add",
            "--cacheinfo",
            &entry,
        ];
        let (worked, _, stderr) = self.run(&args, b"", &[])?;
        if worked {
            Ok(())
        } else if stderr.contains("appears as both a file and as a directory") {
            Err(Refusal::Rule(format!(
                "a file or a folder is in the way of {path}"
            )))
        } else if stderr.contains("Invalid path") {
            Err(Refusal::Rule(format!(
                "a name Git will not store at {path}"
            )))
        } else {
            Err(anyhow!("git update-index failed: {stderr}").into())
        }
    }

    fn remove(&self, path: &str) -> Result<()> {
        let mut entry = format!("0 {NO_OBJECT}\t{path}").into_bytes();
        entry.push(0);
        self.line(&["update-index", "-z", "--index-info"], &entry, &[])?;
        Ok(())
    }

    /// Moves `main` to `commit` if it is still `head`. Returns `false` when it is not, because a
    /// push landed.
    fn swap(&self, commit: &str, head: Option<&str>, machine: &str) -> Result<bool> {
        let reason = format!("mneme: edit by {machine}");
        let old = head.unwrap_or(NO_OBJECT);
        let args = ["update-ref", "-m", &reason, MAIN, commit, old];
        let (worked, _, stderr) = self.run(&args, b"", &[])?;
        if worked {
            return Ok(true);
        }
        let (_, now, _) = self.run(&["rev-parse", "-q", "--verify", MAIN], b"", &[])?;
        if Some(now.as_str()) == head || (now.is_empty() && head.is_none()) {
            bail!("git update-ref failed: {stderr}");
        }
        Ok(false)
    }
}
