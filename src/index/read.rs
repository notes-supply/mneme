//! Pages, events, windows and texts, as the reading API asks for them.

use anyhow::Result;
use gix::ObjectId;
use rusqlite::types::Value;
use rusqlite::{OptionalExtension, params, params_from_iter};
use serde::{Deserialize, Serialize};

use super::{Index, PAGE_COLUMNS, Page, PageId, VERSION_SELECT, Version, page_row, version_row};

const DAY: i64 = 86_400;
/// Versions with gaps under this many seconds belong to one session.
const SESSION_GAP: i64 = 30 * 60;
const WINDOWS: [Option<u32>; 4] = [Some(7), Some(30), Some(90), None];

/// What a lookup did not find. The reading API answers these with 404.
#[derive(Debug, thiserror::Error)]
pub enum Missing {
    #[error("no page has this id")]
    Page,
    #[error("no commit on main has this id")]
    Commit,
    #[error("the page has no text at this commit")]
    Text,
}

/// Which page a question is on.
#[derive(Clone, Debug, Serialize)]
pub struct QuestionPage {
    pub id: PageId,
    pub path: String,
    pub title: String,
}

/// A list item under an "Open questions" heading of a live page.
#[derive(Clone, Debug, Serialize)]
pub struct Question {
    pub page: QuestionPage,
    pub text: String,
    /// When the page last changed.
    pub updated: i64,
}

/// The order of a page listing.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Sort {
    Title,
    Updated,
    Created,
    Versions,
    Words,
    Churn,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Order {
    Asc,
    Desc,
}

/// Bounds on pages. Each one that is set must hold.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Filter {
    pub changed_within_days: Option<u32>,
    pub created_within_days: Option<u32>,
    pub min_versions: Option<u32>,
    pub max_versions: Option<u32>,
    pub min_words: Option<u32>,
    pub max_words: Option<u32>,
}

/// Which pages a listing shows, in what order, and which slice of them.
#[derive(Clone, Debug)]
pub struct Listing {
    /// `None` is by title for pages, and by score for search.
    pub sort: Option<Sort>,
    /// `None` is ascending by title and descending by everything else.
    pub order: Option<Order>,
    pub limit: u32,
    pub offset: u32,
    pub deleted: bool,
    pub filter: Filter,
    /// The time the day filters count back from.
    pub now: i64,
}

/// What a page saw in the last `days` days, or in all time when `days` is `None`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Window {
    pub days: Option<u32>,
    pub versions: u32,
    pub sessions: u32,
    pub added: u64,
    pub removed: u64,
}

/// The text of a page at one point of its history.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct End {
    /// The commit whose event left this text. `None` before the page existed.
    pub commit: Option<String>,
    /// `None` when there is no text: before the page existed, or after it was deleted.
    pub blob: Option<ObjectId>,
}

/// The two texts a diff compares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diff {
    pub from: End,
    pub to: End,
}

impl Listing {
    /// The conditions of the listing and their values, in the order of their placeholders.
    pub(super) fn conditions(&self) -> (Vec<&'static str>, Vec<Value>) {
        let filter = &self.filter;
        let since = |days: u32| Value::Integer(self.now - i64::from(days) * DAY);
        let count = |n: u32| Value::Integer(n.into());
        let bounds = [
            (filter.changed_within_days.map(since), "p.updated >= ?"),
            (filter.created_within_days.map(since), "p.created >= ?"),
            (filter.min_versions.map(count), "p.versions >= ?"),
            (filter.max_versions.map(count), "p.versions <= ?"),
            (filter.min_words.map(count), "p.words >= ?"),
            (filter.max_words.map(count), "p.words <= ?"),
        ];
        let mut conditions = Vec::new();
        let mut values = Vec::new();
        if !self.deleted {
            conditions.push("p.deleted IS NULL");
        }
        for (value, condition) in bounds {
            if let Some(value) = value {
                conditions.push(condition);
                values.push(value);
            }
        }
        (conditions, values)
    }

    /// The `ORDER BY` terms. `score` is the search score, when there is one.
    pub(super) fn order_by(&self, score: Option<&'static str>) -> String {
        let (column, default) = match (self.sort, score) {
            (None, Some(score)) => (score, Order::Desc),
            (None | Some(Sort::Title), _) => ("p.title COLLATE NOCASE", Order::Asc),
            (Some(Sort::Updated), _) => ("p.updated", Order::Desc),
            (Some(Sort::Created), _) => ("p.created", Order::Desc),
            (Some(Sort::Versions), _) => ("p.versions", Order::Desc),
            (Some(Sort::Words), _) => ("p.words", Order::Desc),
            (Some(Sort::Churn), _) => ("p.churn", Order::Desc),
        };
        let order = match self.order.unwrap_or(default) {
            Order::Asc => "ASC",
            Order::Desc => "DESC",
        };
        format!("{column} {order}, p.path, p.rowid")
    }

    pub(super) fn page_values(&self) -> [Value; 2] {
        [self.limit.into(), self.offset.into()]
    }
}

pub(super) fn where_clause(conditions: &[&str]) -> String {
    if conditions.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", conditions.join(" AND "))
    }
}

impl Index {
    /// The page with this id, deleted or not.
    ///
    /// # Errors
    ///
    /// [`Missing::Page`] when no page has this id.
    pub fn page(&self, id: &PageId) -> Result<Page> {
        let page = self
            .db
            .query_row(
                &format!("SELECT {PAGE_COLUMNS} FROM pages p WHERE p.id = ?1"),
                [id.as_str()],
                page_row,
            )
            .optional()?;
        Ok(page.ok_or(Missing::Page)?)
    }

    /// The pages of a listing, and how many pages match it in all.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn pages(&self, listing: &Listing) -> Result<(u32, Vec<Page>)> {
        let (conditions, mut values) = listing.conditions();
        let filter = where_clause(&conditions);
        let total = self.db.query_row(
            &format!("SELECT count(*) FROM pages p{filter}"),
            params_from_iter(&values),
            |row| row.get(0),
        )?;
        values.extend(listing.page_values());
        let mut statement = self.db.prepare_cached(&format!(
            "SELECT {PAGE_COLUMNS} FROM pages p{filter} ORDER BY {} LIMIT ? OFFSET ?",
            listing.order_by(None)
        ))?;
        let pages = statement
            .query_map(params_from_iter(&values), page_row)?
            .collect::<Result<_, _>>()?;
        Ok((total, pages))
    }

    /// Open questions of live pages, newest page first and then in page order, and how many
    /// there are in all.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn questions(&self, limit: u32, offset: u32) -> Result<(u32, Vec<Question>)> {
        let total = self.db.query_row(
            "SELECT count(*) FROM questions q JOIN pages p ON p.rowid = q.page \
             WHERE p.deleted IS NULL",
            [],
            |row| row.get(0),
        )?;
        let mut statement = self.db.prepare_cached(
            "SELECT p.id, p.path, p.title, q.text, p.updated FROM questions q \
             JOIN pages p ON p.rowid = q.page WHERE p.deleted IS NULL \
             ORDER BY p.updated DESC, p.path, p.rowid, q.position LIMIT ?1 OFFSET ?2",
        )?;
        let questions = statement
            .query_map(params![limit, offset], |row| {
                Ok(Question {
                    page: QuestionPage {
                        id: PageId(row.get(0)?),
                        path: row.get(1)?,
                        title: row.get(2)?,
                    },
                    text: row.get(3)?,
                    updated: row.get(4)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok((total, questions))
    }

    /// Events newest first, of one page or of all, at or after `since`, and how many there are
    /// in all.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn events(
        &self,
        page: Option<&PageId>,
        since: Option<i64>,
        limit: u32,
        offset: u32,
    ) -> Result<(u32, Vec<Version>)> {
        let page = page.map(PageId::as_str);
        let filter = "WHERE (?1 IS NULL OR p.id = ?1) AND (?2 IS NULL OR c.time >= ?2)";
        let total = self.db.query_row(
            &format!(
                "SELECT count(*) FROM versions v JOIN commits c ON c.seq = v.commit_seq \
                 JOIN pages p ON p.rowid = v.page {filter}"
            ),
            params![page, since],
            |row| row.get(0),
        )?;
        let mut statement = self.db.prepare_cached(&format!(
            "{VERSION_SELECT} {filter} ORDER BY v.seq DESC LIMIT ?3 OFFSET ?4"
        ))?;
        let events = statement
            .query_map(params![page, since, limit, offset], version_row)?
            .collect::<Result<_, _>>()?;
        Ok((total, events))
    }

    /// Versions, sessions and words of a page in the last 7, 30 and 90 days and in all time.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn windows(&self, page: &PageId, now: i64) -> Result<Vec<Window>> {
        let mut statement = self.db.prepare_cached(
            "SELECT c.time, v.added, v.removed FROM versions v \
             JOIN commits c ON c.seq = v.commit_seq JOIN pages p ON p.rowid = v.page \
             WHERE p.id = ?1 AND v.version ORDER BY c.time",
        )?;
        let versions: Vec<(i64, u32, u32)> = statement
            .query_map([page.as_str()], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<Result<_, _>>()?;
        Ok(WINDOWS
            .iter()
            .map(|&days| window(&versions, days, now))
            .collect())
    }

    /// The text of a page at `commit`, or now when `commit` is `None`.
    ///
    /// # Errors
    ///
    /// [`Missing`] when the page or commit is unknown, or the page did not exist yet.
    pub fn text(&self, page: &PageId, commit: Option<&str>) -> Result<End> {
        let rowid = self.rowid(page)?;
        let seq = self.seq(commit)?;
        Ok(self
            .end("v.commit_seq <= ?2", rowid, seq)?
            .ok_or(Missing::Text)?)
    }

    /// The texts to compare. `to` defaults to now. `from` defaults to the text before the last
    /// change of the text at or before `to`.
    ///
    /// # Errors
    ///
    /// [`Missing`] when the page or a commit is unknown, or the page did not exist at a commit.
    pub fn diff(&self, page: &PageId, from: Option<&str>, to: Option<&str>) -> Result<Diff> {
        let rowid = self.rowid(page)?;
        let to_seq = self.seq(to)?;
        let to = self
            .end("v.commit_seq <= ?2", rowid, to_seq)?
            .ok_or(Missing::Text)?;
        let from = if let Some(commit) = from {
            self.end("v.commit_seq <= ?2", rowid, self.seq(Some(commit))?)?
                .ok_or(Missing::Text)?
        } else {
            let change: Option<i64> = self
                .db
                .query_row(
                    "SELECT seq FROM versions WHERE page = ?1 AND commit_seq <= ?2 \
                     AND (version OR kind = 'deleted') ORDER BY seq DESC LIMIT 1",
                    params![rowid, to_seq],
                    |row| row.get(0),
                )
                .optional()?;
            match change {
                Some(change) => self.end("v.seq < ?2", rowid, change)?.unwrap_or_default(),
                None => End::default(),
            }
        };
        Ok(Diff { from, to })
    }

    fn rowid(&self, page: &PageId) -> Result<i64> {
        let rowid = self
            .db
            .query_row(
                "SELECT rowid FROM pages WHERE id = ?1",
                [page.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        Ok(rowid.ok_or(Missing::Page)?)
    }

    /// The position of a commit on main, or past the end for `None`.
    fn seq(&self, commit: Option<&str>) -> Result<i64> {
        let Some(commit) = commit else {
            return Ok(i64::MAX);
        };
        let seq = self
            .db
            .query_row("SELECT seq FROM commits WHERE id = ?1", [commit], |row| {
                row.get(0)
            })
            .optional()?;
        Ok(seq.ok_or(Missing::Commit)?)
    }

    /// The text left by the last event of a page that meets `condition`, which compares
    /// against `?2`.
    fn end(&self, condition: &'static str, rowid: i64, bound: i64) -> Result<Option<End>> {
        let end = self
            .db
            .query_row(
                &format!(
                    "SELECT c.id, v.blob FROM versions v JOIN commits c ON c.seq = v.commit_seq \
                     WHERE v.page = ?1 AND {condition} ORDER BY v.seq DESC LIMIT 1"
                ),
                params![rowid, bound],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()?;
        end.map(|(commit, blob)| {
            Ok(End {
                commit: Some(commit),
                blob: blob
                    .map(|blob| ObjectId::from_hex(blob.as_bytes()))
                    .transpose()?,
            })
        })
        .transpose()
    }
}

fn window(versions: &[(i64, u32, u32)], days: Option<u32>, now: i64) -> Window {
    let since = days.map_or(i64::MIN, |days| now - i64::from(days) * DAY);
    let inside: Vec<_> = versions
        .iter()
        .filter(|(time, ..)| *time >= since)
        .collect();
    let gaps = inside
        .windows(2)
        .filter(|pair| pair[1].0 - pair[0].0 >= SESSION_GAP)
        .count();
    Window {
        days,
        versions: inside.len().try_into().unwrap_or(u32::MAX),
        sessions: if inside.is_empty() {
            0
        } else {
            (gaps + 1).try_into().unwrap_or(u32::MAX)
        },
        added: inside.iter().map(|(_, added, _)| u64::from(*added)).sum(),
        removed: inside.iter().map(|(.., removed)| u64::from(*removed)).sum(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_under_thirty_minutes_apart_are_one_session() {
        let versions = [
            (0, 1, 0),
            (29 * 60, 2, 1),
            (59 * 60, 3, 0),
            (200 * DAY, 4, 4),
        ];
        let all = window(&versions, None, 201 * DAY);
        assert_eq!((all.versions, all.sessions), (4, 3));
        assert_eq!((all.added, all.removed), (10, 5));
        let week = window(&versions, Some(7), 201 * DAY);
        assert_eq!((week.versions, week.sessions, week.added), (1, 1, 4));
        let none = window(&[], Some(7), 0);
        assert_eq!((none.versions, none.sessions), (0, 0));
    }
}
