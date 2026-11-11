//! Mneme holds the wiki's Git repository, serves it over HTTPS and answers read requests.

use std::fmt::Write as _;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use serde_json::json;

pub mod api;
pub mod index;
pub mod log;
pub mod markdown;
pub mod repo;
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
}

/// Runs the command named on the command line.
pub fn main() -> ExitCode {
    let result = match Cli::parse().command {
        Command::Serve(args) => tokio::runtime::Runtime::new()
            .map_err(anyhow::Error::from)
            .and_then(|runtime| runtime.block_on(serve::run(args))),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            log::emit("failed", json!({ "error": format!("{error:#}") }));
            ExitCode::FAILURE
        }
    }
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
