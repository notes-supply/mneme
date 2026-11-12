//! Machine tokens. The tokens file maps each machine's name to the SHA-256 of its token, in
//! hexadecimal, as one JSON object. A plain hash gives the machine full access, and an object
//! says what it may do: `{ "laptop": "9f86d0…", "mirror": { "sha256": "2c26b4…", "access":
//! "read" } }`.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use axum::http::HeaderValue;
use serde_json::Value;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// What a machine's token lets it do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    /// Fetch, report and read the API, but not push or edit.
    Read,
    /// Everything, as a plain hash in the tokens file gives.
    Write,
}

/// One machine and its token's hash.
#[derive(Debug)]
struct Holder {
    name: String,
    hash: [u8; 64],
    access: Access,
}

/// The machines that may use Git over HTTPS.
#[derive(Debug)]
pub struct Tokens {
    machines: Vec<Holder>,
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
    /// with `sha256` and an optional `access` of `read` or `write`, or a hash is not 64
    /// hexadecimal characters.
    pub fn parse(text: &str) -> Result<Self> {
        let entries: BTreeMap<String, Value> = serde_json::from_str(text)?;
        let machines = entries
            .into_iter()
            .map(|(name, entry)| {
                let (sha256, access) = match entry {
                    Value::String(sha256) => (sha256, Access::Write),
                    Value::Object(fields) => scoped(&name, fields)?,
                    _ => bail!("the entry for {name} is neither a hash nor an object"),
                };
                let sha256 = sha256.to_ascii_lowercase();
                let hash = <[u8; 64]>::try_from(sha256.as_bytes())
                    .ok()
                    .filter(|hash| hash.iter().all(u8::is_ascii_hexdigit));
                let Some(hash) = hash else {
                    bail!("the hash for {name} is not 64 hexadecimal characters");
                };
                Ok(Holder { name, hash, access })
            })
            .collect::<Result<_>>()?;
        Ok(Self { machines })
    }

    /// The name of every machine, in order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.machines.iter().map(|holder| holder.name.as_str())
    }

    /// The machine whose token is in an `Authorization: Bearer` header, and what it may do.
    /// Every stored hash is compared, in constant time.
    #[must_use]
    pub fn machine(&self, authorization: Option<&HeaderValue>) -> Option<(&str, Access)> {
        let token = authorization?.as_bytes().strip_prefix(b"Bearer ")?;
        let presented = crate::hex(&Sha256::digest(token));
        let mut found = None;
        for holder in &self.machines {
            if bool::from(holder.hash.ct_eq(presented.as_bytes())) {
                found = Some((holder.name.as_str(), holder.access));
            }
        }
        found
    }
}

/// The hash and access of an entry that is an object: `sha256`, and `access` of `read` or
/// `write`, which is the default.
fn scoped(name: &str, fields: serde_json::Map<String, Value>) -> Result<(String, Access)> {
    let mut sha256 = None;
    let mut access = Access::Write;
    for (key, value) in fields {
        match (key.as_str(), value) {
            ("sha256", Value::String(hash)) => sha256 = Some(hash),
            ("access", Value::String(said)) if said == "read" => access = Access::Read,
            ("access", Value::String(said)) if said == "write" => access = Access::Write,
            ("access", value) => {
                bail!("the access of {name} is {value}, not \"read\" or \"write\"")
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
            .machines
            .into_iter()
            .map(|holder| (holder.name, holder.access))
            .collect())
    }

    #[test]
    fn a_plain_hash_writes_and_an_object_says_what_its_token_may_do() {
        let text = format!(
            r#"{{ "laptop": "{HASH}", "mirror": {{ "sha256": "{HASH}", "access": "read" }},
                "editor": {{ "sha256": "{}", "access": "write" }}, "tablet": {{ "sha256": "{HASH}" }} }}"#,
            HASH.to_ascii_uppercase()
        );
        assert_eq!(
            access(&text).unwrap(),
            [
                ("editor".to_owned(), Access::Write),
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
                format!(r#"{{ "m": {{ "sha256": "{HASH}", "access": "admin" }} }}"#),
                "admin",
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
}
