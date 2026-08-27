//! Thin clap dispatcher. Every subcommand prints JSON to stdout and nothing else,
//! so output is pipeable into `jq` and diffable between runs.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, NaiveDate, Utc};
use clap::{Parser, Subcommand};
use ib_spread_trader::config::StrategyConfig;
use ib_spread_trader::data::{
    ChainSeries, Snapshot, chain_coverage, snapshot::default_snapshot_path,
};
use ib_spread_trader::{engine, report};
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
    /// Replay a strategy over the snapshot and report what it did.
    Backtest {
        /// Path to a strategy TOML file.
        #[arg(long)]
        config: PathBuf,
        /// Also write trades.csv and equity.csv here.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Include every closed trade in the JSON, not just the summary.
        #[arg(long)]
        trades: bool,
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
        Command::Backtest {
            config,
            out,
            trades,
        } => {
            let strategy_config = StrategyConfig::load(&config)?;
            let run_config = strategy_config.run_config();

            let mut runs = Vec::new();
            let mut skipped = Vec::new();
            for symbol in &strategy_config.symbols {
                let series = ChainSeries::load(&snapshot, symbol, strategy_config.expiry)?;
                if series.is_empty() {
                    // A symbol with no two-sided bars is not a zero-return run;
                    // averaging it in as one would dilute every real result.
                    skipped.push(symbol.clone());
                    continue;
                }
                // A fresh strategy per symbol: signals carry indicator state.
                let mut strategy = strategy_config.strategy()?;
                runs.push(engine::run_symbol(&series, &mut strategy, &run_config)?);
            }
            if runs.is_empty() {
                bail!(
                    "no symbol in the config has two-sided option bars for {}; try `catalog`",
                    strategy_config.expiry
                );
            }

            let mut built = report::build(&strategy_config.name, strategy_config.expiry, &runs);
            if !skipped.is_empty() {
                built.caveats.push(format!(
                    "{} symbol(s) had no two-sided option bars and were not run: {}.",
                    skipped.len(),
                    skipped.join(", ")
                ));
            }
            if let Some(dir) = &out {
                report::write_csvs(dir, &built, &runs)?;
            }

            let mut value = serde_json::to_value(&built)?;
            if !trades && let Some(object) = value.as_object_mut() {
                object.remove("trades");
                object.insert(
                    "trades_written".into(),
                    serde_json::json!(built.trades.len()),
                );
            }
            if let Some(object) = value.as_object_mut() {
                object.insert("config".into(), serde_json::to_value(&strategy_config)?);
                if let Some(dir) = &out {
                    object.insert(
                        "csv_dir".into(),
                        serde_json::json!(dir.display().to_string()),
                    );
                }
            }
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
    }
    Ok(())
}
