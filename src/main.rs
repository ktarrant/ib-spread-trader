//! Thin clap dispatcher. Every subcommand prints JSON to stdout and nothing else,
//! so output is pipeable into `jq` and diffable between runs.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, NaiveDate, Utc};
use clap::{Parser, Subcommand};
use ib_spread_trader::data::{
    ChainSeries, Snapshot, chain_coverage, snapshot::default_snapshot_path,
};
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
    /// Dump one resolved ladder — forward, IV and delta per strike — for eyeballing.
    Chain {
        #[arg(long)]
        symbol: String,
        #[arg(long)]
        expiry: NaiveDate,
        /// RFC3339 with a `+00:00` offset, e.g. 2026-08-19T15:00:00+00:00.
        /// Defaults to the last bar in the chain.
        #[arg(long)]
        at: Option<String>,
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
        Command::Chain { symbol, expiry, at } => {
            let series = ChainSeries::load(&snapshot, &symbol, expiry)?;
            if series.is_empty() {
                bail!("no two-sided option bars for {symbol} {expiry}; try `catalog`");
            }
            let resolved = match &at {
                Some(text) => {
                    let ts: DateTime<Utc> = DateTime::parse_from_rfc3339(text)
                        .with_context(|| format!("`--at {text}` is not RFC3339"))?
                        .with_timezone(&Utc);
                    series.snapshot_at(ts).with_context(|| {
                        format!("no bar at {text} for {} {expiry}", series.symbol)
                    })?
                }
                None => series.snapshot(series.len() - 1).expect("non-empty"),
            };
            let output = serde_json::json!({
                "symbol": series.symbol,
                "expiry": series.expiry,
                "expiry_instant": series.expiry_instant(),
                "bars_in_chain": series.len(),
                "option_quality": series.option_quality,
                "underlying_quality": series.underlying_quality,
                "snapshot": resolved,
            });
            println!("{}", serde_json::to_string_pretty(&output)?);
        }
    }
    Ok(())
}
