//! What the server knows of each machine in the tokens file, in memory only: when it last made
//! an authenticated Git request, and what its autosave service last reported.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};

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

#[derive(Default)]
struct Record {
    seen: Option<i64>,
    report: Option<(i64, Report)>,
}

/// Every machine in the tokens file, by name.
pub struct Machines {
    records: Mutex<BTreeMap<String, Record>>,
}

impl Machines {
    /// No machine seen and none reported yet.
    pub fn new<'a>(names: impl IntoIterator<Item = &'a str>) -> Self {
        let records = names
            .into_iter()
            .map(|name| (name.to_owned(), Record::default()))
            .collect();
        Self {
            records: Mutex::new(records),
        }
    }

    /// Notes that `name` made an authenticated Git request at `now`.
    pub fn saw(&self, name: &str, now: i64) {
        self.with(name, |record| record.seen = Some(now));
    }

    /// Keeps `report` as the latest from `name`, received at `now`.
    pub fn report(&self, name: &str, report: Report, now: i64) {
        self.with(name, |record| record.report = Some((now, report)));
    }

    /// Every machine by name, each compared with the server's `head`.
    #[must_use]
    pub fn list(&self, head: Option<&str>) -> Vec<Health> {
        let records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        records
            .iter()
            .map(|(name, record)| {
                let report = record.report.as_ref();
                let reported_head = report.and_then(|(_, report)| report.head.clone());
                Health {
                    name: name.clone(),
                    seen: record.seen,
                    reported: report.map(|(time, _)| *time),
                    state: report.map(|(_, report)| report.state.clone()),
                    detail: report.map(|(_, report)| report.detail.clone()),
                    current: report.is_some() && reported_head.as_deref() == head,
                    head: reported_head,
                    skipped: report.map(|(_, report)| report.skipped.clone()),
                    skipped_more: report.map(|(_, report)| report.skipped_more),
                }
            })
            .collect()
    }

    fn with(&self, name: &str, change: impl FnOnce(&mut Record)) {
        let mut records = self.records.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(record) = records.get_mut(name) {
            change(record);
        }
    }
}
