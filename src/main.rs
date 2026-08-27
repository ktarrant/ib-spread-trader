//! Thin clap dispatcher. Every subcommand prints JSON to stdout and nothing else,
//! so output is pipeable into `jq` and diffable between runs.

use anyhow::Result;
use clap::{Parser, Subcommand};
use ib_spread_trader::data::{Snapshot, chain_coverage, snapshot::default_snapshot_path};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "ib-spread-trader",
    about = "Backtest modular option-spread strategies against a capture snapshot",
    version
)]
struct Cli {
    /// Snapshot to read. Defaults to the newest in ~/Documents/Options Database.
    #[arg(long, global = true, env = "IB_SNAPSHOT")]
    db: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Report what is backtestable: chains with two-sided quotes, and their extent.
    Catalog {
        /// Include chains too thin to fit a forward or select strikes against.
        #[arg(long)]
        all: bool,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let path = match cli.db {
        Some(path) => path,
        None => default_snapshot_path()?,
    };
    let snapshot = Snapshot::open(&path)?;

    match cli.command {
        Command::Catalog { all } => {
            let mut chains = chain_coverage(&snapshot)?;
            let total = chains.len();
            if !all {
                chains.retain(|chain| chain.is_tradeable());
            }
            let output = serde_json::json!({
                "snapshot": snapshot.path().display().to_string(),
                "schema_version": snapshot.schema_version()?,
                "chains_total": total,
                "chains_shown": chains.len(),
                "chains": chains,
            });
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
    }
    Ok(())
}
