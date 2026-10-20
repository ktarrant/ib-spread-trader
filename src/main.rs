//! Thin clap dispatcher. Every subcommand prints JSON to stdout and nothing else,
//! so output is pipeable into `jq` and diffable between runs.

use anyhow::{Context, Result, bail};
use chrono::{DateTime, NaiveDate, Utc};
use clap::{Parser, Subcommand};
use ib_spread_trader::config::{AccountMode, StrategyConfig};
use ib_spread_trader::data::{
    ChainSeries, NewsSentiment, Snapshot, chain_coverage, news, snapshot::default_snapshot_path,
};
use ib_spread_trader::{engine, intraday, report};
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
    /// Day-trade the stock tape off the initial balance or prior value area.
    Intraday {
        /// Path to an intraday TOML; defaults to the initial-balance rules.
        #[arg(long)]
        config: Option<PathBuf>,
        /// Include every trade in the JSON, not just the summaries.
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
        Command::Intraday { config, trades } => {
            let cfg = match config {
                Some(path) => intraday::IntradayConfig::load(&path)?,
                None => intraday::IntradayConfig::default(),
            };
            let report = intraday::run(&snapshot, &cfg, trades)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
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
                "bar_minutes": series.bar_minutes,
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
            let run_config = strategy_config.run_config()?;
            // The news cache sits beside the snapshots, under its fixed name.
            let sentiment_gate = match strategy_config.execution.sentiment_gate_half_life_hours {
                None => None,
                Some(hours) => {
                    let dir = snapshot
                        .path()
                        .parent()
                        .context("snapshot has no directory")?;
                    Some(engine::SentimentGate {
                        news: std::sync::Arc::new(NewsSentiment::load(dir.join(news::CACHE_NAME))?),
                        half_life: chrono::Duration::seconds((hours * 3600.0).round() as i64),
                    })
                }
            };
            let strategy = || -> Result<engine::Strategy> {
                let mut s = strategy_config.strategy()?;
                if let (Some(gate), engine::Strategy::Ranked(r)) = (&sentiment_gate, &mut s) {
                    r.sentiment_gate = Some(gate.clone());
                }
                Ok(s)
            };

            // Load every symbol's chains once, skipping those with no two-sided
            // option bars (a zero-return run would dilute every real result).
            let expiries = strategy_config.expiries()?;
            let mut symbols: Vec<Vec<ChainSeries>> = Vec::new();
            let mut skipped = Vec::new();
            let mut missing_chains = Vec::new();
            for symbol in &strategy_config.symbols {
                let mut chains = Vec::new();
                for &expiry in &expiries {
                    let series = ChainSeries::load(&snapshot, symbol, expiry)?;
                    if series.is_empty() {
                        missing_chains.push(format!("{symbol} {expiry}"));
                    } else {
                        chains.push(series);
                    }
                }
                if chains.is_empty() {
                    skipped.push(symbol.clone());
                } else {
                    symbols.push(chains);
                }
            }
            if symbols.is_empty() {
                bail!(
                    "no symbol in the config has two-sided option bars for {expiries:?}; try `catalog`"
                );
            }
            // One timeline needs one bar grid: a 15-minute and a 30-minute bar
            // starting together do not close together, so they cannot be marked
            // as one instant. Finer chains are folded to the coarsest.
            let bar_minutes = symbols
                .iter()
                .flatten()
                .map(|s| s.bar_minutes)
                .max()
                .unwrap_or(30);
            let mut folded = Vec::new();
            for series in symbols.iter_mut().flatten() {
                if series.bar_minutes < bar_minutes {
                    folded.push(format!(
                        "{} {} ({}-min)",
                        series.symbol, series.expiry, series.bar_minutes
                    ));
                    series.coarsen(bar_minutes);
                }
            }

            // The shared account runs all symbols against one portfolio over a
            // merged timeline; the per-symbol account runs each independently.
            // Both yield: the report, per-symbol runs (for the CSV logs), and the
            // equity curve to write.
            let (mut built, runs, curve) = if strategy_config.account_mode()? == AccountMode::Shared
            {
                let mut strategies: Vec<engine::Strategy> = symbols
                    .iter()
                    .map(|_| strategy())
                    .collect::<anyhow::Result<_>>()?;
                let shared = engine::run_shared(&symbols, &mut strategies, &run_config)?;
                // The earliest chain: `expiry` alone is unset when a dataset lists several.
                let built = report::build_shared(&strategy_config.name, expiries[0], &shared);
                let views = report::shared_views(&strategy_config.name, expiries[0], &shared);
                (built, views, shared.equity_curve)
            } else {
                // Single-expiry by validation: one chain per symbol.
                let mut runs = Vec::new();
                for series in symbols.iter().flatten() {
                    // A fresh strategy per symbol: signals carry indicator state.
                    let mut strategy = strategy()?;
                    runs.push(engine::run_symbol(series, &mut strategy, &run_config)?);
                }
                let built = report::build(&strategy_config.name, strategy_config.expiry, &runs);
                let curve = report::combined_curve(&runs);
                (built, runs, curve)
            };
            // A symbol with some chains but not all ran on what it had; say which.
            let partial: Vec<&String> = missing_chains
                .iter()
                .filter(|chain| !skipped.iter().any(|s| chain.starts_with(&format!("{s} "))))
                .collect();
            if !partial.is_empty() {
                built.caveats.push(format!(
                    "{} chain(s) had no two-sided option bars and were not run: {}.",
                    partial.len(),
                    partial
                        .iter()
                        .map(|c| c.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
            built.bar_minutes = bar_minutes;
            if !folded.is_empty() {
                built.caveats.push(format!(
                    "{} chain(s) were folded to {bar_minutes}-minute bars to share one timeline \
                     with coarser ones: {}.",
                    folded.len(),
                    folded.join(", ")
                ));
            }
            if !skipped.is_empty() {
                built.caveats.push(format!(
                    "{} symbol(s) had no two-sided option bars and were not run: {}.",
                    skipped.len(),
                    skipped.join(", ")
                ));
            }
            // The resolved blocks, not the labels: a report describes the run
            // that happened. `ib-option-viz` reads `config.model` / `config.exit`
            // as tables, and a label there both breaks it and hides which
            // candidate universe actually ran.
            let reported_config = serde_json::to_value(strategy_config.resolved()?)?;
            let labels = serde_json::to_value(strategy_config.labels())?;
            if let Some(dir) = &out {
                report::write_csvs(dir, &built, &runs, &curve)?;
                // Always write the full JSON (with trades) for downstream tools.
                let mut full = serde_json::to_value(&built)?;
                if let Some(obj) = full.as_object_mut() {
                    obj.insert("config".into(), reported_config.clone());
                    obj.insert("labels".into(), labels.clone());
                    // Which data the run read: publish-results.sh cuts its
                    // bundle from this same file.
                    obj.insert("snapshot".into(), snapshot.provenance()?);
                }
                std::fs::write(
                    dir.join("backtest.json"),
                    serde_json::to_string_pretty(&full)?,
                )?;
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
                object.insert("config".into(), reported_config);
                object.insert("labels".into(), labels);
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
