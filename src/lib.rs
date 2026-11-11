//! Mneme holds the wiki's Git repository, serves it over HTTPS and answers read requests.

use std::process::ExitCode;

use clap::{Parser, Subcommand};

pub mod index;
pub mod markdown;
pub mod repo;
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
    Serve,
}

/// Runs the command named on the command line.
pub fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Serve => ExitCode::SUCCESS,
    }
}
