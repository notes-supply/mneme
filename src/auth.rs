//! Machine tokens. The tokens file maps each machine's name to the SHA-256 of its token, in
//! hexadecimal, as one JSON object. A plain hash gives the machine write access, and an object
//! says what it may do: `{ "laptop": "9f86d0…", "mirror": { "sha256": "2c26b4…", "access":
//! "read" } }`. An admin token, which only the tokens file can give, may also add machines while
//! the server runs, and remove them. Those are kept in a JSON file beside the repository, as the
//! machines' state is, and every check sees a change at once.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock, RwLockReadGuard};

use anyhow::{Context, Result, anyhow, bail};
use axum::Json;
use axum::extract::{Request, State};
use axum::http::header::{AUTHORIZATION, WWW_AUTHENTICATE};
use axum::http::{HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::log;

/// The longest name of a machine added through the API.
const MAX_NAME: usize = 64;

/// The machine a request came from, for the request log.
#[derive(Clone, Debug)]
pub struct Machine(pub String);

/// What a machine's token lets it do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// Fetch, report and read the API, but not push or edit.
    Read,
    /// Everything but managing machines, as a plain hash in the tokens file gives.
    Write,
    /// Everything, and add and remove machines while the server runs.
    Admin,
}

impl Access {
    /// Whether the token may push and edit.
    #[must_use]
    pub fn may_write(self) -> bool {
        matches!(self, Self::Write | Self::Admin)
    }
}

/// What a machine added through the API may do. It is never admin.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Granted {
    Read,
    Write,
}

impl From<Granted> for Access {
    fn from(granted: Granted) -> Self {
        match granted {
            Granted::Read => Self::Read,
            Granted::Write => Self::Write,
        }
    }
}

/// One machine of the tokens file and its token's hash.
#[derive(Debug)]
struct Holder {
    name: String,
    hash: [u8; 64],
    access: Access,
}

/// One machine added through the API, as its file keeps it.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Stored {
    /// 64 lowercase hexadecimal characters.
    sha256: String,
    access: Granted,
    /// When it was added, in seconds since the Unix epoch.
    added: i64,
}

/// One machine added through the API, as `GET /v1/tokens` lists it: never its hash.
#[derive(Clone, Debug, Serialize)]
pub struct Listed {
    pub name: String,
    pub access: Granted,
    pub added: i64,
}

/// Why a machine was not added or removed.
#[derive(Debug)]
pub enum Refused {
    /// The name breaks the rule for names added through the API.
    Name(String),
    /// The hash is not 64 hexadecimal characters.
    Hash,
    /// The tokens file has the name, so the API may not change it.
    Configured(String),
    /// Another machine already has the hash.
    Taken(String),
    /// No machine of that name was added through the API.
    NotAdded(String),
    /// The file could not be written, and nothing changed.
    Failed(anyhow::Error),
}

/// The machines that may use Git over HTTPS and the API: those of the tokens file, and those
/// added through the API.
#[derive(Debug)]
pub struct Tokens {
    configured: Vec<Holder>,
    added: RwLock<BTreeMap<String, Stored>>,
    /// Where the added machines are kept, or `None` when the API may not add any.
    file: Option<PathBuf>,
    /// Held while the file is written, so that one change at a time is made.
    changing: Mutex<()>,
}

impl Tokens {
    /// Reads the tokens file.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be read or [`Tokens::parse`] refuses it.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading the tokens file {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("reading the tokens file {}", path.display()))
    }

    /// Reads the tokens from the environment variable `name`, which holds the same JSON as the
    /// tokens file.
    ///
    /// # Errors
    ///
    /// Fails when the variable is not set or not UTF-8, or [`Tokens::parse`] refuses it.
    pub fn from_env(name: &str) -> Result<Self> {
        let text = std::env::var(name)
            .with_context(|| format!("reading the tokens from the environment variable {name}"))?;
        Self::parse(&text)
            .with_context(|| format!("reading the tokens from the environment variable {name}"))
    }

    /// Reads the JSON of a tokens file.
    ///
    /// # Errors
    ///
    /// Fails when the text is not a JSON object, an entry is neither a string nor an object
    /// with `sha256` and an optional `access` of `read`, `write` or `admin`, or a hash is not 64
    /// hexadecimal characters.
    pub fn parse(text: &str) -> Result<Self> {
        let entries: BTreeMap<String, Value> = serde_json::from_str(text)?;
        let configured = entries
            .into_iter()
            .map(|(name, entry)| {
                let (sha256, access) = match entry {
                    Value::String(sha256) => (sha256, Access::Write),
                    Value::Object(fields) => scoped(&name, fields)?,
                    _ => bail!("the entry for {name} is neither a hash nor an object"),
                };
                let Some(hash) = hash(&sha256) else {
                    bail!("the hash for {name} is not 64 hexadecimal characters");
                };
                Ok(Holder { name, hash, access })
            })
            .collect::<Result<_>>()?;
        Ok(Self {
            configured,
            added: RwLock::default(),
            file: None,
            changing: Mutex::new(()),
        })
    }

    /// The same tokens, with the machines added through the API kept in `file`, and those it
    /// holds already. A missing file holds none. A name it holds that the tokens file now has
    /// is dropped from it, so the tokens file always wins.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be read or holds a bad entry, rather than start without
    /// machines that were added and then lose them on the next change.
    pub fn with_added(mut self, file: PathBuf) -> Result<Self> {
        let reading = || format!("reading {}", file.display());
        let mut stored: BTreeMap<String, Stored> = match std::fs::read(&file) {
            Ok(bytes) => serde_json::from_slice(&bytes).with_context(reading)?,
            Err(error) if error.kind() == ErrorKind::NotFound => BTreeMap::new(),
            Err(error) => return Err(error).with_context(reading),
        };
        for (name, entry) in &mut stored {
            if let Some(problem) = name_problem(name) {
                bail!("{}: the name {name:?} {problem}", reading());
            }
            let Some(hash) = hash(&entry.sha256) else {
                bail!(
                    "{}: the hash for {name} is not 64 hexadecimal characters",
                    reading()
                );
            };
            entry.sha256 = String::from_utf8_lossy(&hash).into_owned();
        }
        let dropped: Vec<String> = stored
            .keys()
            .filter(|name| self.configures(name))
            .cloned()
            .collect();
        for name in &dropped {
            stored.remove(name);
        }
        if !dropped.is_empty() {
            crate::machines::write(&file, &serde_json::to_vec(&stored)?)?;
        }
        log::emit(
            "tokens",
            json!({ "file": file, "added": stored.len(), "dropped": dropped }),
        );
        self.added = RwLock::new(stored);
        self.file = Some(file);
        Ok(self)
    }

    /// The name of every machine: those of the tokens file, then those added through the API.
    #[must_use]
    pub fn names(&self) -> Vec<String> {
        let configured = self.configured.iter().map(|holder| holder.name.clone());
        configured.chain(self.read().keys().cloned()).collect()
    }

    /// The machine whose token is in an `Authorization: Bearer` header, and what it may do.
    /// Every stored hash is compared, in constant time, and the tokens file wins.
    #[must_use]
    pub fn machine(&self, authorization: Option<&HeaderValue>) -> Option<(String, Access)> {
        let token = authorization?.as_bytes().strip_prefix(b"Bearer ")?;
        let presented = crate::hex(&Sha256::digest(token));
        let presented = presented.as_bytes();
        let mut configured = None;
        for holder in &self.configured {
            if bool::from(holder.hash.ct_eq(presented)) {
                configured = Some((holder.name.as_str(), holder.access));
            }
        }
        let mut added = None;
        for (name, entry) in self.read().iter() {
            if bool::from(entry.sha256.as_bytes().ct_eq(presented)) {
                added = Some((name.clone(), entry.access.into()));
            }
        }
        configured
            .map(|(name, access)| (name.to_owned(), access))
            .or(added)
    }

    /// Every machine added through the API, by name.
    #[must_use]
    pub fn added(&self) -> Vec<Listed> {
        self.read()
            .iter()
            .map(|(name, entry)| Listed {
                name: name.clone(),
                access: entry.access,
                added: entry.added,
            })
            .collect()
    }

    /// Adds the machine `name` with the token whose SHA-256 is `sha256`, or replaces its hash
    /// and access, at `now`. Returns it, and whether it replaced one. The file is written
    /// before any check sees the change.
    ///
    /// # Errors
    ///
    /// Refuses a bad name or hash, a name of the tokens file, a hash another machine has, and
    /// fails when the file cannot be written.
    pub fn add(
        &self,
        name: &str,
        sha256: &str,
        access: Granted,
        now: i64,
    ) -> Result<(Listed, bool), Refused> {
        if let Some(problem) = name_problem(name) {
            return Err(Refused::Name(format!("the name {name:?} {problem}")));
        }
        if self.configures(name) {
            return Err(Refused::Configured(name.to_owned()));
        }
        let hash = hash(sha256).ok_or(Refused::Hash)?;
        self.change(|added| {
            let taken = self.configured.iter().any(|holder| holder.hash == hash)
                || added
                    .iter()
                    .any(|(other, entry)| other != name && entry.sha256.as_bytes() == hash);
            if taken {
                return Err(Refused::Taken(name.to_owned()));
            }
            let entry = Stored {
                sha256: String::from_utf8_lossy(&hash).into_owned(),
                access,
                added: now,
            };
            let replaced = added.insert(name.to_owned(), entry).is_some();
            let listed = Listed {
                name: name.to_owned(),
                access,
                added: now,
            };
            Ok((listed, replaced))
        })
    }

    /// Removes the machine `name` that was added through the API. Its next request is refused.
    ///
    /// # Errors
    ///
    /// Refuses a name of the tokens file and one that was not added, and fails when the file
    /// cannot be written.
    pub fn remove(&self, name: &str) -> Result<(), Refused> {
        if self.configures(name) {
            return Err(Refused::Configured(name.to_owned()));
        }
        self.change(|added| match added.remove(name) {
            Some(_) => Ok(()),
            None => Err(Refused::NotAdded(name.to_owned())),
        })
    }

    /// Makes `edit` on a copy of the added machines, writes the copy to the file, and only then
    /// lets checks see it.
    fn change<T>(
        &self,
        edit: impl FnOnce(&mut BTreeMap<String, Stored>) -> Result<T, Refused>,
    ) -> Result<T, Refused> {
        let file = self
            .file
            .as_ref()
            .ok_or_else(|| Refused::Failed(anyhow!("this server keeps no added machines")))?;
        let _one = self.changing.lock().unwrap_or_else(PoisonError::into_inner);
        let mut next = self.read().clone();
        let made = edit(&mut next)?;
        let json = serde_json::to_vec(&next).map_err(|error| Refused::Failed(error.into()))?;
        crate::machines::write(file, &json).map_err(Refused::Failed)?;
        *self.added.write().unwrap_or_else(PoisonError::into_inner) = next;
        Ok(made)
    }

    /// Whether the tokens file has `name`, in any case.
    fn configures(&self, name: &str) -> bool {
        self.configured
            .iter()
            .any(|holder| holder.name.eq_ignore_ascii_case(name))
    }

    fn read(&self) -> RwLockReadGuard<'_, BTreeMap<String, Stored>> {
        self.added.read().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The hash as 64 lowercase hexadecimal characters, or `None` when it is not 64 hexadecimal
/// characters.
fn hash(sha256: &str) -> Option<[u8; 64]> {
    let hash = <[u8; 64]>::try_from(sha256.to_ascii_lowercase().as_bytes()).ok()?;
    hash.iter().all(u8::is_ascii_hexdigit).then_some(hash)
}

/// What is wrong with `name` as the name of a machine added through the API, or `None`. It is
/// 1 to 64 lowercase letters, digits, `.`, `_` and `-`, and starts with a letter or a digit, so
/// that it is one word in a commit's committer, a log line and a URL.
fn name_problem(name: &str) -> Option<&'static str> {
    let alphanumeric = |byte: &u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    if name.is_empty() || name.len() > MAX_NAME {
        Some("is empty or over 64 characters")
    } else if !name.as_bytes().first().is_some_and(alphanumeric) {
        Some("does not start with a lowercase letter or a digit")
    } else if !name
        .bytes()
        .all(|byte| alphanumeric(&byte) || matches!(byte, b'.' | b'_' | b'-'))
    {
        Some("holds a character other than a-z, 0-9, '.', '_' and '-'")
    } else {
        None
    }
}

/// The answer to a request without a machine's token: no body, since it says nothing about why.
#[must_use]
pub fn unauthorized() -> Response {
    (StatusCode::UNAUTHORIZED, [(WWW_AUTHENTICATE, "Bearer")]).into_response()
}

/// Lets a request through only with a machine's token, of any access, and names that machine
/// in the request log. It guards the reading API of a server run with `--private-api`.
pub async fn any_token(
    State(tokens): State<Arc<Tokens>>,
    request: Request,
    next: Next,
) -> Response {
    let Some((machine, _)) = tokens.machine(request.headers().get(AUTHORIZATION)) else {
        return unauthorized();
    };
    let mut response = next.run(request).await;
    response.extensions_mut().insert(Machine(machine));
    response
}

/// What a route asks of a token, beyond being one the server knows.
#[derive(Clone, Copy, Debug)]
pub enum Need {
    /// A token that may write: the editing routes.
    Write,
    /// An admin token: the routes that manage machines.
    Admin,
}

/// Lets a request through only with a token that does what `need` says, and gives the handler
/// its machine. Any other token is 403 with `{ "error" }`, and no token or an unknown one 401
/// with no body.
pub async fn guard(
    State((tokens, need)): State<(Arc<Tokens>, Need)>,
    mut request: Request,
    next: Next,
) -> Response {
    let Some((machine, access)) = tokens.machine(request.headers().get(AUTHORIZATION)) else {
        return unauthorized();
    };
    let machine = Machine(machine);
    let refusal = match need {
        Need::Write if !access.may_write() => Some("has a read-only token, which may not edit"),
        Need::Admin if access != Access::Admin => Some(
            "has no admin token, which only the tokens file gives, so it may not manage machines",
        ),
        _ => None,
    };
    let mut response = if let Some(refusal) = refusal {
        let error = format!("{} {refusal}", machine.0);
        (StatusCode::FORBIDDEN, Json(json!({ "error": error }))).into_response()
    } else {
        request.extensions_mut().insert(machine.clone());
        next.run(request).await
    };
    response.extensions_mut().insert(machine);
    response
}

/// The hash and access of an entry that is an object: `sha256`, and `access` of `read`,
/// `write`, which is the default, or `admin`.
fn scoped(name: &str, fields: serde_json::Map<String, Value>) -> Result<(String, Access)> {
    let mut sha256 = None;
    let mut access = Access::Write;
    for (key, value) in fields {
        match (key.as_str(), value) {
            ("sha256", Value::String(hash)) => sha256 = Some(hash),
            ("access", Value::String(said)) if said == "read" => access = Access::Read,
            ("access", Value::String(said)) if said == "write" => access = Access::Write,
            ("access", Value::String(said)) if said == "admin" => access = Access::Admin,
            ("access", value) => {
                bail!("the access of {name} is {value}, not \"read\", \"write\" or \"admin\"")
            }
            ("sha256", value) => bail!("the sha256 of {name} is {value}, not a string"),
            (key, _) => bail!("the entry for {name} has {key:?}, and takes only sha256 and access"),
        }
    }
    let sha256 = sha256.with_context(|| format!("the entry for {name} has no sha256"))?;
    Ok((sha256, access))
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

    fn access(text: &str) -> Result<Vec<(String, Access)>> {
        let tokens = Tokens::parse(text)?;
        Ok(tokens
            .configured
            .into_iter()
            .map(|holder| (holder.name, holder.access))
            .collect())
    }

    #[test]
    fn a_plain_hash_writes_and_an_object_says_what_its_token_may_do() {
        let text = format!(
            r#"{{ "laptop": "{HASH}", "mirror": {{ "sha256": "{HASH}", "access": "read" }},
                "editor": {{ "sha256": "{}", "access": "write" }}, "tablet": {{ "sha256": "{HASH}" }},
                "keeper": {{ "sha256": "{HASH}", "access": "admin" }} }}"#,
            HASH.to_ascii_uppercase()
        );
        assert_eq!(
            access(&text).unwrap(),
            [
                ("editor".to_owned(), Access::Write),
                ("keeper".to_owned(), Access::Admin),
                ("laptop".to_owned(), Access::Write),
                ("mirror".to_owned(), Access::Read),
                ("tablet".to_owned(), Access::Write),
            ]
        );
    }

    #[test]
    fn an_unknown_key_or_a_bad_value_is_refused_and_named() {
        for (text, said) in [
            (
                format!(r#"{{ "m": {{ "sha256": "{HASH}", "access": "owner" }} }}"#),
                "owner",
            ),
            (
                format!(r#"{{ "m": {{ "sha256": "{HASH}", "scope": "read" }} }}"#),
                "scope",
            ),
            (r#"{ "m": { "access": "read" } }"#.to_owned(), "sha256"),
            (
                format!(r#"{{ "m": {{ "sha256": "{HASH}", "access": true }} }}"#),
                "access",
            ),
            (r#"{ "m": "abc" }"#.to_owned(), "64 hexadecimal"),
            (
                format!(r#"{{ "m": "{}" }}"#, "g".repeat(64)),
                "64 hexadecimal",
            ),
            (r#"{ "m": 7 }"#.to_owned(), "neither a hash nor an object"),
            (format!(r#"["{HASH}"]"#), "map"),
        ] {
            let error = format!("{:#}", access(&text).unwrap_err());
            assert!(error.contains(said), "{text}: {error}");
        }
    }

    #[test]
    fn a_name_added_through_the_api_is_one_lowercase_word() {
        for name in ["a", "laptop-2", "ada.lovelace", "x_1", &"a".repeat(64)] {
            assert_eq!(name_problem(name), None, "{name}");
        }
        for name in [
            "",
            &"a".repeat(65),
            "Laptop",
            "-laptop",
            ".hidden",
            "two words",
            "a/b",
            "a<b>",
            "café",
            "line\nbreak",
        ] {
            assert!(name_problem(name).is_some(), "{name:?}");
        }
    }
}
