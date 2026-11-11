CREATE TABLE commits (
    seq INTEGER PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    time INTEGER NOT NULL,
    author TEXT NOT NULL,
    overlap TEXT
);

CREATE TABLE pages (
    rowid INTEGER PRIMARY KEY,
    id TEXT NOT NULL UNIQUE,
    path TEXT NOT NULL,
    title TEXT NOT NULL,
    words INTEGER NOT NULL,
    created INTEGER NOT NULL,
    updated INTEGER NOT NULL,
    versions INTEGER NOT NULL,
    churn INTEGER NOT NULL,
    deleted INTEGER,
    blob TEXT
);

CREATE INDEX pages_live_path ON pages (path) WHERE deleted IS NULL;

CREATE TABLE versions (
    seq INTEGER PRIMARY KEY,
    commit_seq INTEGER NOT NULL REFERENCES commits (seq),
    page INTEGER NOT NULL REFERENCES pages (rowid),
    path TEXT NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('created', 'edited', 'renamed', 'deleted')),
    from_path TEXT,
    version INTEGER NOT NULL,
    words INTEGER NOT NULL,
    added INTEGER NOT NULL,
    removed INTEGER NOT NULL,
    blob TEXT
);

CREATE INDEX versions_page ON versions (page, seq);

CREATE VIRTUAL TABLE search USING fts5 (
    title,
    body,
    tokenize = 'unicode61 remove_diacritics 2'
);
