//! Files that are not pages and their events, as the reading API asks for them.

use anyhow::Result;
use rusqlite::types::Value;
use rusqlite::{OptionalExtension, params, params_from_iter};
use serde::{Deserialize, Serialize};

use super::read::where_clause;
use super::{Index, Kind, Missing, Order};
use crate::media;

/// A file of the wiki that is not a page, followed across renames that keep its bytes.
#[derive(Clone, Debug, Serialize)]
pub struct File {
    /// The current path, or the last one for a deleted file.
    pub path: String,
    /// The size in bytes, or the last one for a deleted file.
    pub size: u64,
    /// The media type its bytes are sent with.
    #[serde(rename = "type")]
    pub media: &'static str,
    /// Author time of the commit that created the file.
    pub created: i64,
    /// Author time of the file's newest event.
    pub updated: i64,
    /// How many events changed its bytes, the creation included.
    pub versions: u32,
    /// The author of the newest event.
    pub author: String,
    /// The commit of the newest event.
    pub commit: String,
    /// Author time of the commit that deleted the file.
    pub deleted: Option<i64>,
}

/// One event in a file's history.
#[derive(Clone, Debug, Serialize)]
pub struct FileEvent {
    pub commit: String,
    /// The file's path after the event.
    pub path: String,
    /// Author time.
    pub time: i64,
    pub author: String,
    #[serde(flatten)]
    pub kind: Kind,
    /// The size in bytes after the event, which is 0 after a deletion.
    pub size: u64,
}

/// The order of a file listing.
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FileSort {
    Path,
    Updated,
    Size,
}

/// Which live files a listing shows, in what order, and which slice of them.
#[derive(Clone, Debug, Default)]
pub struct FileListing {
    /// Only files inside this folder, at any depth.
    pub under: Option<String>,
    /// Words that must each be in the path, without case.
    pub words: Option<String>,
    /// `None` is by path.
    pub sort: Option<FileSort>,
    /// `None` is ascending by path and descending by everything else.
    pub order: Option<Order>,
    pub limit: u32,
    pub offset: u32,
}

const FILE_SELECT: &str = "SELECT f.rowid, f.path, f.size, f.created, c.time, f.versions, c.author, c.id, \
     f.deleted FROM files f JOIN file_events e ON e.seq = f.last \
     JOIN commits c ON c.seq = e.commit_seq";

const EVENT_SELECT: &str = "SELECT c.id, e.path, c.time, c.author, e.kind, e.from_path, e.size \
     FROM file_events e JOIN commits c ON c.seq = e.commit_seq";

impl FileListing {
    /// The conditions of the listing and their values, in the order of their placeholders.
    fn conditions(&self) -> (Vec<&'static str>, Vec<Value>) {
        let mut conditions = vec!["f.deleted IS NULL"];
        let mut values = Vec::new();
        let folder = self.under.as_deref().map(|under| under.trim_matches('/'));
        if let Some(folder) = folder.filter(|folder| !folder.is_empty()) {
            // Every path inside the folder sorts from `folder/` up to `folder0`, the character
            // after the slash, so `brand` holds `brand/logo.svg` and not `brandy.txt`.
            conditions.push("f.path >= ? AND f.path < ?");
            values.push(Value::Text(format!("{folder}/")));
            values.push(Value::Text(format!("{folder}0")));
        }
        for word in self.words.iter().flat_map(|words| words.split_whitespace()) {
            conditions.push("instr(f.folded, ?) > 0");
            values.push(Value::Text(word.to_lowercase()));
        }
        (conditions, values)
    }

    fn order_by(&self) -> String {
        let (column, default) = match self.sort {
            None | Some(FileSort::Path) => ("f.path", Order::Asc),
            Some(FileSort::Updated) => ("c.time", Order::Desc),
            Some(FileSort::Size) => ("f.size", Order::Desc),
        };
        let order = match self.order.unwrap_or(default) {
            Order::Asc => "ASC",
            Order::Desc => "DESC",
        };
        format!("{column} {order}, f.path, f.rowid")
    }
}

impl Index {
    /// The live files of a listing, and how many files match it in all.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn files(&self, listing: &FileListing) -> Result<(u32, Vec<File>)> {
        let (conditions, mut values) = listing.conditions();
        let filter = where_clause(&conditions);
        let total = self.db.query_row(
            &format!("SELECT count(*) FROM files f{filter}"),
            params_from_iter(&values),
            |row| row.get(0),
        )?;
        values.extend([listing.limit.into(), listing.offset.into()]);
        let mut statement = self.db.prepare_cached(&format!(
            "{FILE_SELECT}{filter} ORDER BY {} LIMIT ? OFFSET ?",
            listing.order_by()
        ))?;
        let files = statement
            .query_map(params_from_iter(&values), |row| {
                file_row(row).map(|(_, file)| file)
            })?
            .collect::<Result<_, _>>()?;
        Ok((total, files))
    }

    /// The live file at `path`, or else the file most recently deleted there, with the key its
    /// events are found by.
    ///
    /// # Errors
    ///
    /// [`Missing::File`] when no file is at the path and none was deleted there.
    pub fn file(&self, path: &str) -> Result<(i64, File)> {
        let file = self
            .db
            .query_row(
                &format!(
                    "{FILE_SELECT} WHERE f.path = ?1 \
                     ORDER BY f.deleted IS NOT NULL, f.last DESC LIMIT 1"
                ),
                [path],
                file_row,
            )
            .optional()?;
        Ok(file.ok_or(Missing::File)?)
    }

    /// Events newest first, of one file or of all, at or after `since` and before `before`, and
    /// how many there are in all.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn file_events(
        &self,
        file: Option<i64>,
        since: Option<i64>,
        before: Option<i64>,
        limit: u32,
        offset: u32,
    ) -> Result<(u32, Vec<FileEvent>)> {
        let filter = "WHERE (?1 IS NULL OR e.file = ?1) AND (?2 IS NULL OR c.time >= ?2) \
                      AND (?3 IS NULL OR c.time < ?3)";
        let total = self.db.query_row(
            &format!(
                "SELECT count(*) FROM file_events e JOIN commits c ON c.seq = e.commit_seq \
                 {filter}"
            ),
            params![file, since, before],
            |row| row.get(0),
        )?;
        let mut statement = self.db.prepare_cached(&format!(
            "{EVENT_SELECT} {filter} ORDER BY e.seq DESC LIMIT ?4 OFFSET ?5"
        ))?;
        let events = statement
            .query_map(params![file, since, before, limit, offset], event_row)?
            .collect::<Result<_, _>>()?;
        Ok((total, events))
    }
}

fn file_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(i64, File)> {
    let path: String = row.get(1)?;
    let file = File {
        media: media::of(&path).content_type,
        path,
        size: row.get(2)?,
        created: row.get(3)?,
        updated: row.get(4)?,
        versions: row.get(5)?,
        author: row.get(6)?,
        commit: row.get(7)?,
        deleted: row.get(8)?,
    };
    Ok((row.get(0)?, file))
}

fn event_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<FileEvent> {
    Ok(FileEvent {
        commit: row.get(0)?,
        path: row.get(1)?,
        time: row.get(2)?,
        author: row.get(3)?,
        kind: Kind::read(row, 4, 5)?,
        size: row.get(6)?,
    })
}
