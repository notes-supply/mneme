//! Mneme holds the wiki's Git repository, serves it over HTTPS and answers read requests.

use std::fmt::Write as _;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use clap::{Parser, Subcommand};
use serde_json::json;

pub mod api;
pub mod auth;
pub mod autosave;
pub mod git_http;
pub mod hook;
pub mod index;
pub mod log;
pub mod machines;
pub mod markdown;
pub mod repo;
pub mod rules;
pub mod serve;
pub mod text;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve the repository over HTTPS and the reading API.
    Serve(serve::Args),
    /// Save, sync and push a wiki folder, so that writers only edit files.
    Autosave {
        #[command(subcommand)]
        command: autosave::Command,
    },
    /// Run a Git hook. The server installs it in the repository.
    Hook {
        #[command(subcommand)]
        hook: Hook,
    },
}

#[derive(Subcommand)]
enum Hook {
    /// Refuse a push that would break the wiki's rules.
    PreReceive,
}

/// Runs the command named on the command line.
pub fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Command::Serve(args) => tokio::runtime::Runtime::new()
            .map_err(anyhow::Error::from)
            .and_then(|runtime| runtime.block_on(serve::run(args))),
        Command::Autosave { command } => autosave::main(command),
        Command::Hook {
            hook: Hook::PreReceive,
        } => return hook::pre_receive(),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            log::emit("failed", json!({ "error": format!("{error:#}") }));
            ExitCode::FAILURE
        }
    }
}

/// The time in seconds since the Unix epoch.
pub(crate) fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs().try_into().unwrap_or(i64::MAX))
}

/// Lowercase hexadecimal.
pub(crate) fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
            let _ = write!(out, "{byte:02x}");
            out
        })
}
