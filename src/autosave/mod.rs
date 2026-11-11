//! `mneme autosave`: makes a wiki folder behave like a note app with autosave. Writers only
//! edit files. The service commits, fetches, replays and pushes, one tick at a time.

use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// A machine's token. Its `Debug` leaves the token out.
#[derive(Clone)]
pub struct Token(pub String);

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(..)")
    }
}

/// What one service works on, and how often.
#[derive(Clone, Debug)]
pub struct Config {
    /// The wiki folder, a clone of the server's repository.
    pub folder: PathBuf,
    /// The server's base URL, such as `https://mneme.example`.
    pub server: String,
    pub token: Token,
    /// The machine's name, as in the server's tokens file. Autosave commits carry it.
    pub machine: String,
    /// How long every changed file must have been still before a save.
    pub quiet: Duration,
    /// How often to fetch when nothing else asks for it.
    pub fetch_every: Duration,
    /// How often to report the state when it has not changed.
    pub report_every: Duration,
}

/// What a tick did, or why it did nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Idle,
    Saved,
    Pulled,
    Pushed,
    /// The server cannot be reached. The detail is Git's error.
    Offline(String),
    /// The folder is in a state the service must not touch, or the server refused a push.
    Blocked(String),
    /// A save would have deleted most pages, so nothing was committed.
    Stopped(String),
}

/// One service and the little it remembers between ticks.
pub struct Autosave {
    config: Config,
}

impl Autosave {
    /// A service that has not ticked yet.
    #[must_use]
    pub fn new(config: Config) -> Self {
        Self { config }
    }

    /// The folder this service saves.
    #[must_use]
    pub fn folder(&self) -> &std::path::Path {
        &self.config.folder
    }

    /// Does one round of guard, save, fetch, integrate, push and report, at `now`.
    pub fn tick(&mut self, now: SystemTime) -> Outcome {
        let _ = now;
        Outcome::Idle
    }
}
