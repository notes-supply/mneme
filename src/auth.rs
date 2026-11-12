//! Machine tokens. The tokens file maps each machine's name to the SHA-256 of its token, in
//! hexadecimal, as one JSON object: `{ "laptop": "9f86d0…" }`.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, bail};
use axum::http::HeaderValue;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

/// The machines that may use Git over HTTPS.
#[derive(Debug)]
pub struct Tokens {
    machines: Vec<(String, [u8; 64])>,
}

impl Tokens {
    /// Reads the tokens file.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be read, is not a JSON object of strings, or a hash is not
    /// 64 hexadecimal characters.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading the tokens file {}", path.display()))?;
        let entries: BTreeMap<String, String> = serde_json::from_str(&text)
            .with_context(|| format!("reading the tokens file {}", path.display()))?;
        let machines = entries
            .into_iter()
            .map(|(machine, hash)| {
                let hash = hash.to_ascii_lowercase();
                let Ok(hash) = <[u8; 64]>::try_from(hash.as_bytes()) else {
                    bail!("the hash for {machine} is not 64 hexadecimal characters");
                };
                if !hash.iter().all(u8::is_ascii_hexdigit) {
                    bail!("the hash for {machine} is not 64 hexadecimal characters");
                }
                Ok((machine, hash))
            })
            .collect::<Result<_>>()?;
        Ok(Self { machines })
    }

    /// The name of every machine, in order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.machines.iter().map(|(machine, _)| machine.as_str())
    }

    /// The machine whose token is in an `Authorization: Bearer` header. Every stored hash is
    /// compared, in constant time.
    #[must_use]
    pub fn machine(&self, authorization: Option<&HeaderValue>) -> Option<&str> {
        let token = authorization?.as_bytes().strip_prefix(b"Bearer ")?;
        let presented = crate::hex(&Sha256::digest(token));
        let mut found = None;
        for (machine, hash) in &self.machines {
            if bool::from(hash.ct_eq(presented.as_bytes())) {
                found = Some(machine.as_str());
            }
        }
        found
    }
}
