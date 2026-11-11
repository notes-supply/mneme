//! What the server knows of each machine in the tokens file: when it last made an authenticated
//! Git request, and what its autosave service last reported. It is kept in a JSON file beside the
//! repository's own files, outside its refs and objects, so it survives a restart.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use anyhow::{Context, Result};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::log;

/// The header an autosave service sends its report in, on a Git request.
pub const REPORT_HEADER: &str = "Mneme-Report";
/// The longest report header value.
pub const MAX_REPORT: usize = 4 << 10;
/// The most skipped paths a report lists.
pub const MAX_SKIPPED: usize = 20;
const MAX_STATE: usize = 64;
const MAX_DETAIL: usize = 1024;
const MAX_REASON: usize = 64;

/// What an autosave service reports, as JSON in base64 in its [`REPORT_HEADER`].
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Report {
    /// The service's state, such as `idle` or `blocked`.
    pub state: String,
    /// Why, for a state that needs a reason. Otherwise empty.
    pub detail: String,
    /// The clone's `main`, or `None` when it has no commit yet.
    pub head: Option<String>,
    /// Paths the service leaves out of its saves, by path, at most [`MAX_SKIPPED`].
    pub skipped: Vec<Skipped>,
    /// How many more it leaves out than `skipped` lists.
    pub skipped_more: usize,
}

/// A path the service leaves in the folder rather than commit, since the server would refuse
/// it or Git cannot store it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Skipped {
    pub path: String,
    pub reason: String,
}

/// Why a report header cannot be stored.
#[derive(Debug)]
pub enum Unreadable {
    TooLarge,
    /// Not base64, or not JSON once decoded.
    Garbled(String),
    /// JSON with missing or unknown fields, or a bad value.
    Wrong(String),
}

impl Report {
    /// The report as a header value.
    #[must_use]
    pub fn encode(&self) -> String {
        STANDARD.encode(serde_json::to_vec(self).unwrap_or_default())
    }

    /// Reads a report header value.
    ///
    /// # Errors
    ///
    /// Fails when the value is too long, garbled, or not a report that can be stored.
    pub fn decode(value: &[u8]) -> Result<Self, Unreadable> {
        if value.len() > MAX_REPORT {
            return Err(Unreadable::TooLarge);
        }
        let json = STANDARD
            .decode(value)
            .map_err(|error| Unreadable::Garbled(format!("the report is not base64: {error}")))?;
        let report: Self = serde_json::from_slice(&json).map_err(|error| {
            if error.is_data() {
                Unreadable::Wrong(error.to_string())
            } else {
                Unreadable::Garbled(format!("the report is not JSON: {error}"))
            }
        })?;
        match report.problem() {
            Some(problem) => Err(Unreadable::Wrong(problem.to_owned())),
            None => Ok(report),
        }
    }

    /// Why the report cannot be stored, or `None` when it can.
    fn problem(&self) -> Option<&'static str> {
        if self.state.is_empty() || self.state.len() > MAX_STATE {
            return Some("state is empty or too long");
        }
        if self.detail.len() > MAX_DETAIL {
            return Some("detail is too long");
        }
        let commit = |head: &String| {
            head.len() == 40 && head.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        };
        if self.head.as_ref().is_some_and(|head| !commit(head)) {
            return Some("head is not a full commit id");
        }
        if self.skipped.len() > MAX_SKIPPED {
            return Some("skipped lists more than 20 paths");
        }
        if self.skipped.iter().any(|skipped| {
            skipped.path.is_empty()
                || skipped.reason.is_empty()
                || skipped.reason.len() > MAX_REASON
        }) {
            return Some("a skipped path or its reason is empty, or the reason is too long");
        }
        None
    }
}

/// One machine, as `GET /v1/machines` lists it.
#[derive(Debug, Serialize)]
pub struct Health {
    pub name: String,
    /// When the machine last made an authenticated Git request.
    pub seen: Option<i64>,
    /// When its last report arrived.
    pub reported: Option<i64>,
    pub state: Option<String>,
    pub detail: Option<String>,
    pub head: Option<String>,
    pub skipped: Option<Vec<Skipped>>,
    pub skipped_more: Option<usize>,
    /// Whether the reported head is the server's head.
    pub current: bool,
}

/// What the server knows of one machine, as the state file holds it.
#[derive(Default, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
struct Record {
    seen: Option<i64>,
    report: Option<Received>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Received {
    at: i64,
    report: Report,
}

struct State {
    records: BTreeMap<String, Record>,
    /// Whether a record changed since the file was last written.
    changed: bool,
}

/// Every machine in the tokens file, by name.
pub struct Machines {
    state: Mutex<State>,
    file: PathBuf,
}

impl Machines {
    /// The machines named, with what `file` held for them. A missing or unreadable file is
    /// logged once and means that no machine has been seen or has reported yet.
    pub fn load<'a>(names: impl IntoIterator<Item = &'a str>, file: PathBuf) -> Self {
        let mut stored = match read(&file) {
            Ok(stored) => {
                log::emit("machines", json!({ "file": file, "loaded": stored.len() }));
                stored
            }
            Err(error) => {
                log::emit(
                    "machines",
                    json!({ "file": file, "loaded": 0, "error": format!("{error:#}") }),
                );
                BTreeMap::new()
            }
        };
        let records = names
            .into_iter()
            .map(|name| (name.to_owned(), stored.remove(name).unwrap_or_default()))
            .collect();
        Self {
            state: Mutex::new(State {
                records,
                changed: false,
            }),
            file,
        }
    }

    /// Notes that `name` made an authenticated Git request at `now`.
    pub fn saw(&self, name: &str, now: i64) {
        self.with(name, |record| record.seen = Some(now));
    }

    /// Keeps `report` as the latest from `name`, received at `now`.
    pub fn report(&self, name: &str, report: Report, now: i64) {
        self.with(name, |record| {
            record.report = Some(Received { at: now, report });
        });
    }

    /// Every machine by name, each compared with the server's `head`.
    #[must_use]
    pub fn list(&self, head: Option<&str>) -> Vec<Health> {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state
            .records
            .iter()
            .map(|(name, record)| {
                let report = record.report.as_ref().map(|received| &received.report);
                let reported_head = report.and_then(|report| report.head.clone());
                Health {
                    name: name.clone(),
                    seen: record.seen,
                    reported: record.report.as_ref().map(|received| received.at),
                    state: report.map(|report| report.state.clone()),
                    detail: report.map(|report| report.detail.clone()),
                    current: report.is_some() && reported_head.as_deref() == head,
                    head: reported_head,
                    skipped: report.map(|report| report.skipped.clone()),
                    skipped_more: report.map(|report| report.skipped_more),
                }
            })
            .collect()
    }

    /// Writes the state file when a record changed since it was last written, through a
    /// temporary file and a rename, so that a crash leaves the old file or the new one.
    ///
    /// # Errors
    ///
    /// Fails when the file cannot be written. The change is then written on the next call.
    pub fn save(&self) -> Result<()> {
        let json = {
            let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
            if !state.changed {
                return Ok(());
            }
            state.changed = false;
            serde_json::to_vec(&state.records)?
        };
        write(&self.file, &json).inspect_err(|_| {
            self.state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .changed = true;
        })
    }

    fn with(&self, name: &str, change: impl FnOnce(&mut Record)) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(record) = state.records.get_mut(name) {
            change(record);
            state.changed = true;
        }
    }
}

fn read(file: &Path) -> Result<BTreeMap<String, Record>> {
    let text = fs::read(file).with_context(|| format!("reading {}", file.display()))?;
    serde_json::from_slice(&text).with_context(|| format!("reading {}", file.display()))
}

fn write(file: &Path, json: &[u8]) -> Result<()> {
    let folder = file.parent().context("the state file has no folder")?;
    fs::create_dir_all(folder).with_context(|| format!("creating {}", folder.display()))?;
    let temporary = file.with_extension("json.tmp");
    let mut out =
        File::create(&temporary).with_context(|| format!("creating {}", temporary.display()))?;
    out.write_all(json)?;
    out.sync_all()?;
    fs::rename(&temporary, file).with_context(|| format!("writing {}", file.display()))
}
