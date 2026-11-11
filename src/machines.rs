//! What the server knows of each machine in the tokens file, in memory only: when it last made
//! an authenticated Git request, and what its autosave service last reported.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use serde::{Deserialize, Serialize};

const MAX_STATE: usize = 64;
const MAX_DETAIL: usize = 1024;

/// What an autosave service reports with `POST /git/status`.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Report {
    /// The service's state, such as `idle` or `blocked`.
    pub state: String,
    /// Why, for a state that needs a reason. Otherwise empty.
    pub detail: String,
    /// The clone's `main`, or `None` when it has no commit yet.
    pub head: Option<String>,
}

impl Report {
    /// Why the report cannot be stored, or `None` when it can.
    #[must_use]
    pub fn problem(&self) -> Option<&'static str> {
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
