//! Search: plain words, `"a phrase"`, `word*` and `-word`, matched with FTS5 and ranked with
//! `bm25`, the title weighing three times the text.

use std::iter::Peekable;
use std::str::Chars;

use anyhow::Result;
use rusqlite::params_from_iter;
use rusqlite::types::Value;
use serde::Serialize;

use super::read::where_clause;
use super::{Index, Listing, PAGE_COLUMNS, Page, page_row};

const SCORE: &str = "-bm25(search, 3.0, 1.0)";
/// Marks around matched words in a snippet. Indexed text never holds them.
pub(super) const MARKERS: [char; 2] = ['\u{2}', '\u{3}'];

/// A search a reader typed, parsed into terms.
#[derive(Debug, PartialEq, Eq)]
pub struct Query {
    terms: Vec<Term>,
}

#[derive(Debug, PartialEq, Eq)]
struct Term {
    text: String,
    prefix: bool,
    excluded: bool,
}

/// Why a search could not be read. The reading API answers these with 400.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum QueryError {
    #[error("the search has no words")]
    Empty,
    #[error("a quote is not closed")]
    Unclosed,
    #[error("quotes go around a whole phrase")]
    StrayQuote,
    #[error("* goes at the end of a word")]
    Star,
    #[error("- goes right before a word or a phrase")]
    LoneMinus,
    #[error("the search only excludes words")]
    OnlyExcluded,
}

/// A search result.
#[derive(Debug, Serialize)]
pub struct Hit {
    pub page: Page,
    /// Higher is better.
    pub score: f64,
    pub snippet: Vec<Mark>,
}

/// A run of snippet text, and whether it matched the search.
#[derive(Debug, PartialEq, Eq, Serialize)]
pub struct Mark {
    pub text: String,
    pub hit: bool,
}

impl Query {
    /// Parses what a reader typed.
    ///
    /// # Errors
    ///
    /// A [`QueryError`] that says what is wrong.
    pub fn parse(input: &str) -> Result<Self, QueryError> {
        let mut terms = Vec::new();
        let mut chars = input.chars().peekable();
        loop {
            while chars.next_if(|c| c.is_whitespace()).is_some() {}
            if chars.peek().is_none() {
                break;
            }
            let excluded = chars.next_if_eq(&'-').is_some();
            let (text, prefix) = if chars.next_if_eq(&'"').is_some() {
                phrase(&mut chars)?
            } else {
                word(&mut chars)?
            };
            if text.trim().is_empty() {
                return Err(if excluded {
                    QueryError::LoneMinus
                } else {
                    QueryError::Empty
                });
            }
            terms.push(Term {
                text,
                prefix,
                excluded,
            });
        }
        if terms.is_empty() {
            return Err(QueryError::Empty);
        }
        if terms.iter().all(|term| term.excluded) {
            return Err(QueryError::OnlyExcluded);
        }
        Ok(Self { terms })
    }

    /// The FTS5 query. Every term is a quoted string, so no text the reader typed is read as
    /// FTS5 syntax.
    fn fts5(&self) -> String {
        let quoted = |term: &Term| {
            let star = if term.prefix { "*" } else { "" };
            format!("\"{}\"{star}", term.text)
        };
        let included: Vec<_> = self
            .terms
            .iter()
            .filter(|term| !term.excluded)
            .map(quoted)
            .collect();
        let mut query = format!("({})", included.join(" AND "));
        for term in self.terms.iter().filter(|term| term.excluded) {
            query.push_str(" NOT ");
            query.push_str(&quoted(term));
        }
        query
    }
}

/// The rest of a phrase after its opening quote, and whether a `*` follows it.
fn phrase(chars: &mut Peekable<Chars<'_>>) -> Result<(String, bool), QueryError> {
    let mut text = String::new();
    loop {
        match chars.next() {
            Some('"') => break,
            Some(c) => text.push(c),
            None => return Err(QueryError::Unclosed),
        }
    }
    let prefix = chars.next_if_eq(&'*').is_some();
    if chars.peek().is_some_and(|c| !c.is_whitespace()) {
        return Err(QueryError::StrayQuote);
    }
    Ok((text, prefix))
}

/// A word up to the next whitespace, and whether it ends in `*`.
fn word(chars: &mut Peekable<Chars<'_>>) -> Result<(String, bool), QueryError> {
    let mut text = String::new();
    while let Some(c) = chars.next_if(|c| !c.is_whitespace()) {
        text.push(c);
    }
    let prefix = text.ends_with('*');
    if prefix {
        text.pop();
    }
    if text.contains('"') {
        return Err(QueryError::StrayQuote);
    }
    if text.contains('*') || (prefix && text.is_empty()) {
        return Err(QueryError::Star);
    }
    Ok((text, prefix))
}

fn marks(snippet: &str) -> Vec<Mark> {
    snippet
        .split(MARKERS)
        .enumerate()
        .filter(|(_, text)| !text.is_empty())
        .map(|(i, text)| Mark {
            text: text.to_owned(),
            hit: i % 2 == 1,
        })
        .collect()
}

impl Index {
    /// The pages that match a search, and how many match in all.
    ///
    /// # Errors
    ///
    /// Fails on a database error.
    pub fn search(&self, query: &Query, listing: &Listing) -> Result<(u32, Vec<Hit>)> {
        let (mut conditions, mut values) = listing.conditions();
        conditions.insert(0, "search MATCH ?");
        values.insert(0, Value::Text(query.fts5()));
        let filter = where_clause(&conditions);
        let from = "FROM search JOIN pages p ON p.rowid = search.rowid";
        let total = self.db.query_row(
            &format!("SELECT count(*) {from}{filter}"),
            params_from_iter(&values),
            |row| row.get(0),
        )?;
        values.extend(listing.page_values());
        let mut statement = self.db.prepare_cached(&format!(
            "SELECT {PAGE_COLUMNS}, {SCORE}, snippet(search, -1, char(2), char(3), '…', 16) \
             {from}{filter} ORDER BY {} LIMIT ? OFFSET ?",
            listing.order_by(Some(SCORE))
        ))?;
        let hits = statement
            .query_map(params_from_iter(&values), |row| {
                Ok(Hit {
                    page: page_row(row)?,
                    score: row.get(9)?,
                    snippet: marks(&row.get::<_, String>(10)?),
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok((total, hits))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fts5(input: &str) -> String {
        Query::parse(input).unwrap().fts5()
    }

    #[test]
    fn words_phrases_prefixes_and_exclusions_become_quoted_terms() {
        assert_eq!(
            fts5(r#"boat "old harbour" sail* -storm -"dry dock""#),
            r#"("boat" AND "old harbour" AND "sail"*) NOT "storm" NOT "dry dock""#
        );
    }

    #[test]
    fn fts5_syntax_in_a_word_stays_text() {
        assert_eq!(
            fts5("title:x AND (y) NEAR"),
            r#"("title:x" AND "AND" AND "(y)" AND "NEAR")"#
        );
        assert_eq!(fts5("well-known"), r#"("well-known")"#);
    }

    #[test]
    fn broken_searches_say_what_is_wrong() {
        let cases = [
            ("", QueryError::Empty),
            ("   ", QueryError::Empty),
            ("\"open", QueryError::Unclosed),
            ("a\"b", QueryError::StrayQuote),
            ("\"a\"b", QueryError::StrayQuote),
            ("a*b", QueryError::Star),
            ("*", QueryError::Star),
            ("- word", QueryError::LoneMinus),
            ("\"\"", QueryError::Empty),
            ("-word", QueryError::OnlyExcluded),
        ];
        for (input, error) in cases {
            assert_eq!(Query::parse(input), Err(error), "{input:?}");
        }
    }

    #[test]
    fn snippet_markers_split_into_hits() {
        assert_eq!(
            marks("a \u{2}boat\u{3} b"),
            [
                Mark {
                    text: "a ".to_owned(),
                    hit: false
                },
                Mark {
                    text: "boat".to_owned(),
                    hit: true
                },
                Mark {
                    text: " b".to_owned(),
                    hit: false
                },
            ]
        );
    }
}
