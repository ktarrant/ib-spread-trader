//! The bar loop: signal, structure, selector, fill, mark.
//!
//! ## Ordering, and why entries fill on the next bar
//!
//! A signal is computed from a bar's own close. Nobody can trade at a price they
//! are still waiting to observe, so a decision made on bar N executes against
//! bar N+1's quotes. `Timing::SameBar` exists only to measure how much that
//! costs — run the same strategy both ways and the gap is the look-ahead a
//! same-bar backtest would have handed itself.
//!
//! Exits queue the same way. In life a profit target is a resting limit order
//! and would fill sooner, so next-bar exits are pessimistic — which is the right
//! direction to be wrong.
//!
//! ## What happens at the end
//!
//! The option tape stops the day before expiry, so whatever is still open when
//! the bars run out is settled at intrinsic against the stock's expiry-day
//! print. That P&L is reported apart from everything that closed on a quote.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::data::{ChainSeries, ChainSnapshot, Quality};
use crate::execution::{Costs, Reject, Selector};
use crate::portfolio::{ExitReason, Portfolio};
use crate::signal::{MarketView, Signal};
use crate::spread::Spread;
use crate::structure::Structure;

pub mod exits;

pub use exits::ExitRules;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Timing {
    /// Decide on bar N, fill against bar N+1. The honest default.
    #[default]
    NextBar,
    /// Fill at the same bar that produced the decision. For comparison only —
    /// the difference against `NextBar` is the size of the look-ahead.
    SameBar,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct RunConfig {
    pub initial_cash: f64,
    pub costs: Costs,
    pub timing: Timing,
    pub exits: ExitRules,
    /// How many spreads may be open at once. One means the strategy is simply
    /// in or out; without a cap a repeatedly-firing signal stacks positions.
    pub max_open_positions: usize,
    /// Refuse new entries when fewer than this many calendar days remain before
    /// expiry. Prevents opening positions the before-expiry exit would
    /// immediately close.
    pub min_entry_days_before_expiry: Option<u32>,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            initial_cash: 100_000.0,
            costs: Costs::default(),
            timing: Timing::NextBar,
            exits: ExitRules::default(),
            max_open_positions: 1,
            min_entry_days_before_expiry: None,
        }
    }
}

/// One strategy's three stages. Signals are stateful, so each symbol needs its own.
pub struct Strategy {
    pub name: String,
    pub signal: Box<dyn Signal>,
    pub structure: Box<dyn Structure>,
    pub selector: Box<dyn Selector>,
}

/// A decision taken on one bar, to be executed on the next.
#[derive(Debug, Clone)]
enum Intent {
    Open(Spread),
    Close { id: usize, reason: ExitReason },
}

/// Why a bar produced no trade despite a trigger.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Census {
    /// The signal fired.
    pub triggers: usize,
    /// The structure declined the trigger — wrong direction for its right.
    pub declined: usize,
    /// A position was already open and the cap was reached.
    pub at_position_limit: usize,
    /// The selector refused, keyed by `Reject::kind()`.
    pub rejections: BTreeMap<String, usize>,
    /// One worked example per rejection kind, so a census entry can be read.
    pub rejection_examples: BTreeMap<String, String>,
    /// A queued order could not be filled against the bar that came next.
    pub unfillable_intents: usize,
    /// Refused because too close to expiry to open a new position.
    pub too_close_to_expiry: usize,
    /// Closed at intrinsic because option quotes were unavailable.
    pub synthetic_exits: usize,
}

impl Census {
    fn reject(&mut self, reject: &Reject) {
        let kind = reject.kind().to_string();
        *self.rejections.entry(kind.clone()).or_default() += 1;
        self.rejection_examples
            .entry(kind)
            .or_insert_with(|| reject.to_string());
    }

    pub fn total_rejections(&self) -> usize {
        self.rejections.values().sum()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SymbolRun {
    pub symbol: String,
    pub strategy: String,
    pub expiry: chrono::NaiveDate,
    pub bars: usize,
    pub first_ts: Option<DateTime<Utc>>,
    pub last_ts: Option<DateTime<Utc>>,
    pub initial_cash: f64,
    pub final_equity: f64,
    pub census: Census,
    /// Signal-internal gate counts, empty for signals that need no extra census.
    pub signal_diagnostics: BTreeMap<String, usize>,
    pub option_quality: Quality,
    pub underlying_quality: Quality,
    pub portfolio: Portfolio,
    pub equity_curve: Vec<(DateTime<Utc>, f64)>,
    /// The underlying print settlement used, if anything was still open.
    pub settlement_price: Option<f64>,
}

/// Replay one symbol/expiry.
///
/// The main loop iterates over the merged timeline — all timestamps where
/// either option or underlying data exists. On option bars (30-min), the full
/// pipeline runs: fills, marks, exits, signal. On stock-only bars (5-min
/// intermediate ticks), only the signal advances, giving it finer-grained
/// price resolution without inventing option quotes.
///
/// A trigger that fires on a stock-only bar is carried to the next option bar,
/// where it is resolved against that bar's chain — the same way a real trader
/// would see the stock move, decide to act, and then check the option chain
/// at the next available snapshot.
pub fn run_symbol(
    series: &ChainSeries,
    strategy: &mut Strategy,
    config: &RunConfig,
) -> Result<SymbolRun> {
    let mut portfolio = Portfolio::new(config.initial_cash);
    let mut census = Census::default();
    let mut equity_curve = Vec::with_capacity(series.len());
    let mut pending: Vec<Intent> = Vec::new();
    let mut opened_at_index: BTreeMap<usize, usize> = BTreeMap::new();
    let mut carried_trigger: Option<crate::signal::Trigger> = None;

    let timeline = series.merged_timeline();

    for (seq, &(ts, option_index)) in timeline.iter().enumerate() {
        if let Some(index) = option_index {
            // ---- Full option bar: fills, marks, exits, signal ----
            let Some(bar) = series.snapshot(index) else {
                continue;
            };

            // 1. Execute what a previous bar decided.
            for intent in std::mem::take(&mut pending) {
                execute(
                    &intent,
                    &bar,
                    &mut portfolio,
                    config,
                    &mut census,
                    &opened_at_index,
                    seq,
                )?;
                if let Intent::Open(_) = intent
                    && let Some(position) = portfolio.open.last()
                {
                    opened_at_index.insert(position.id, seq);
                }
            }

            // 2. Mark.
            equity_curve.push((bar.ts, portfolio.equity(&bar, &config.costs)));

            // 3. Exits.
            let mut exiting = Vec::new();
            let mut synthetic_exiting: Vec<(usize, ExitReason)> = Vec::new();
            for position in &portfolio.open {
                match crate::execution::exit_credit(&position.spread, &bar) {
                    Ok(credit) => {
                        let gross = credit - position.entry_debit;
                        let ctx = exits::ExitContext {
                            gross_pnl: gross,
                            underlying_price: bar.underlying.price(),
                            bar_ts: bar.ts,
                            expiry_instant: series.expiry_instant(),
                        };
                        if let Some(reason) = config.exits.triggered(position, &ctx) {
                            exiting.push(Intent::Close {
                                id: position.id,
                                reason,
                            });
                        }
                    }
                    Err(_) => {
                        if let Some(reason) = config.exits.time_triggered(
                            position.opened_at,
                            bar.ts,
                            series.expiry_instant(),
                        ) {
                            synthetic_exiting.push((position.id, reason));
                        }
                    }
                }
            }

            for (id, reason) in &synthetic_exiting {
                if let Some(spot) = bar.underlying.price() {
                    let held = seq
                        .saturating_sub(opened_at_index.get(id).copied().unwrap_or(seq));
                    portfolio.close_at_intrinsic(
                        *id,
                        spot,
                        bar.ts,
                        &config.costs,
                        *reason,
                        held,
                    );
                    census.synthetic_exits += 1;
                }
            }

            // 4. Signal — evaluate on this bar, then pick up any trigger carried
            //    from a stock-only bar if the current bar didn't fire.
            let view = MarketView {
                symbol: &series.symbol,
                chain: &bar,
            };
            let current_trigger = strategy.signal.evaluate(&view);
            let trigger = current_trigger.or(carried_trigger.take());
            carried_trigger = None;

            let mut entering = None;
            if let Some(trigger) = trigger {
                census.triggers += 1;
                match strategy.structure.build(&trigger, &view) {
                    None => census.declined += 1,
                    Some(spec) => {
                        let open_after_exits = portfolio.open.len() - exiting.len();
                        if open_after_exits >= config.max_open_positions {
                            census.at_position_limit += 1;
                        } else if let Some(min_days) = config.min_entry_days_before_expiry {
                            let remaining = (series.expiry_instant() - bar.ts).num_days();
                            if remaining < min_days as i64 {
                                census.too_close_to_expiry += 1;
                            } else {
                                match strategy.selector.resolve(&spec, &bar) {
                                    Ok(spread) => entering = Some(Intent::Open(spread)),
                                    Err(reject) => census.reject(&reject),
                                }
                            }
                        } else {
                            match strategy.selector.resolve(&spec, &bar) {
                                Ok(spread) => entering = Some(Intent::Open(spread)),
                                Err(reject) => census.reject(&reject),
                            }
                        }
                    }
                }
            }

            // 5. Queue or execute.
            let decided: Vec<Intent> = exiting.into_iter().chain(entering).collect();
            match config.timing {
                Timing::NextBar => pending = decided,
                Timing::SameBar => {
                    for intent in decided {
                        execute(
                            &intent,
                            &bar,
                            &mut portfolio,
                            config,
                            &mut census,
                            &opened_at_index,
                            seq,
                        )?;
                        if let Intent::Open(_) = intent
                            && let Some(position) = portfolio.open.last()
                        {
                            opened_at_index.insert(position.id, seq);
                        }
                    }
                }
            }
        } else {
            // ---- Stock-only bar: advance the signal, carry any trigger ----
            let Some(bar) = series.stock_only_snapshot(ts) else {
                continue;
            };
            let view = MarketView {
                symbol: &series.symbol,
                chain: &bar,
            };
            let trigger = strategy.signal.evaluate(&view);
            if trigger.is_some() {
                carried_trigger = trigger;
            }
        }
    }

    // Whatever survived the tape settles at intrinsic.
    let settlement = series
        .underlying_asof(series.expiry_instant())
        .and_then(|bar| bar.price());
    if !portfolio.open.is_empty()
        && let Some(price) = settlement
    {
        let ids: Vec<usize> = portfolio.open.iter().map(|p| p.id).collect();
        for id in ids {
            let held = timeline
                .len()
                .saturating_sub(opened_at_index.get(&id).copied().unwrap_or(0));
            portfolio.settle_at_expiry(id, price, series.expiry_instant(), held);
        }
        equity_curve.push((series.expiry_instant(), portfolio.cash));
    }

    let final_equity = portfolio.cash;
    let signal_diagnostics = strategy.signal.diagnostics();
    Ok(SymbolRun {
        symbol: series.symbol.clone(),
        strategy: strategy.name.clone(),
        expiry: series.expiry,
        bars: timeline.len(),
        first_ts: timeline.first().map(|&(ts, _)| ts),
        last_ts: timeline.last().map(|&(ts, _)| ts),
        initial_cash: config.initial_cash,
        final_equity,
        census,
        signal_diagnostics,
        option_quality: series.option_quality,
        underlying_quality: series.underlying_quality,
        portfolio,
        equity_curve,
        settlement_price: settlement,
    })
}

fn execute(
    intent: &Intent,
    bar: &ChainSnapshot,
    portfolio: &mut Portfolio,
    config: &RunConfig,
    census: &mut Census,
    opened_at_index: &BTreeMap<usize, usize>,
    index: usize,
) -> Result<()> {
    match intent {
        Intent::Open(spread) => {
            // The bar that arrives may not quote what the previous one did. A
            // missed fill is a real outcome, counted rather than forced.
            if portfolio.open(spread.clone(), bar, &config.costs).is_err() {
                census.unfillable_intents += 1;
            }
        }
        Intent::Close { id, reason } => {
            let held = index.saturating_sub(opened_at_index.get(id).copied().unwrap_or(index));
            if portfolio
                .close(*id, bar, &config.costs, *reason, held)
                .is_err()
            {
                census.unfillable_intents += 1;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
pub mod testing {
    use super::*;
    use crate::data::Snapshot;
    use crate::pricing;
    use chrono::{Duration, NaiveDate, TimeZone};
    use std::path::PathBuf;

    pub const EXPIRY: (i32, u32, u32) = (2026, 8, 21);
    const VOL: f64 = 0.30;

    pub fn expiry() -> NaiveDate {
        NaiveDate::from_ymd_opt(EXPIRY.0, EXPIRY.1, EXPIRY.2).unwrap()
    }

    /// A price path that drives RSI under 30 and back through it: down hard for
    /// 25 bars, then a sustained recovery.
    pub fn v_shaped_path() -> Vec<f64> {
        let mut prices = vec![100.0];
        for _ in 0..25 {
            prices.push(prices.last().unwrap() - 0.8);
        }
        for _ in 0..55 {
            prices.push(prices.last().unwrap() + 0.6);
        }
        prices
    }

    /// Build a real on-disk snapshot whose option ladder is priced by Black-76
    /// off the given path. Because the generator and the loader agree on the
    /// model, the parity fit recovers the path exactly and every delta is
    /// analytic — so a test failure is the engine's, not the fixture's.
    pub fn snapshot_with(name: &str, path: &[f64], settlement: f64) -> PathBuf {
        let dir = std::env::temp_dir().join("ib-spread-trader-engine-fixtures");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(format!("{name}.sqlite3"));
        let _ = std::fs::remove_file(&file);

        let conn = rusqlite::Connection::open(&file).unwrap();
        conn.pragma_update(None, "user_version", 6i64).unwrap();
        conn.execute_batch(
            "CREATE TABLE contracts (id INTEGER PRIMARY KEY, symbol TEXT, kind TEXT,
                 exchange TEXT, currency TEXT, expiry TEXT, strike REAL, right TEXT);
             CREATE TABLE bars (contract_id INTEGER, ts TEXT, bar_size TEXT, data_type TEXT,
                 open REAL, high REAL, low REAL, close REAL, volume REAL);
             INSERT INTO contracts (id, symbol, kind, exchange, currency, expiry, strike, right)
             VALUES (1, 'TEST', 'stock', 'SMART', 'USD', NULL, NULL, NULL);",
        )
        .unwrap();

        let strikes: Vec<f64> = (35..=65).map(|i| i as f64 * 2.0).collect();
        let expiry_text = expiry().format("%Y-%m-%d").to_string();
        let mut next_id = 2i64;
        let mut ids = Vec::new();
        for &strike in &strikes {
            for right in ["C", "P"] {
                conn.execute(
                    "INSERT INTO contracts (id, symbol, kind, exchange, currency, expiry, strike, right)
                     VALUES (?1, 'TEST', 'option', 'SMART', 'USD', ?2, ?3, ?4)",
                    rusqlite::params![next_id, expiry_text, strike, right],
                )
                .unwrap();
                ids.push((next_id, strike, right == "C"));
                next_id += 1;
            }
        }

        let start = Utc.with_ymd_and_hms(2026, 7, 1, 13, 30, 0).unwrap();
        let expiry_instant = pricing::expiry_instant(expiry()).unwrap();
        conn.execute_batch("BEGIN").unwrap();
        let mut insert = conn
            .prepare(
                "INSERT INTO bars (contract_id, ts, bar_size, data_type, open, high, low, close, volume)
                 VALUES (?1, ?2, '30min', ?3, ?4, ?4, ?4, ?4, 0)",
            )
            .unwrap();

        for (bar, &spot) in path.iter().enumerate() {
            let ts = (start + Duration::minutes(30 * bar as i64))
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, false);
            for (kind, price) in [("trades", spot), ("bid", spot - 0.01), ("ask", spot + 0.01)] {
                insert
                    .execute(rusqlite::params![1i64, &ts, kind, price])
                    .unwrap();
            }
            let t =
                pricing::year_fraction(start + Duration::minutes(30 * bar as i64), expiry_instant);
            for &(id, strike, is_call) in &ids {
                // Floor the mid so a deep wing still quotes a positive book,
                // as a real one does.
                let mid = pricing::black76(spot, strike, t, VOL, is_call).max(0.06);
                insert
                    .execute(rusqlite::params![id, &ts, "bid", mid - 0.05])
                    .unwrap();
                insert
                    .execute(rusqlite::params![id, &ts, "ask", mid + 0.05])
                    .unwrap();
            }
        }

        // The stock print on expiry day that settlement reads.
        let settle_ts = Utc
            .with_ymd_and_hms(EXPIRY.0, EXPIRY.1, EXPIRY.2, 19, 30, 0)
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, false);
        insert
            .execute(rusqlite::params![1i64, &settle_ts, "trades", settlement])
            .unwrap();

        drop(insert);
        conn.execute_batch("COMMIT").unwrap();
        drop(conn);
        file
    }

    /// Like `snapshot_with` but after `dropout_after` bars, only the far
    /// wings keep quoting — the ATM-ish strikes (within ±20 of spot) go
    /// quiet. The chain still has timestamps from the wings, but the
    /// position's specific contracts return `None` from `bar.quote()`.
    pub fn snapshot_with_dropout(
        name: &str,
        path: &[f64],
        settlement: f64,
        dropout_after: usize,
    ) -> PathBuf {
        let dir = std::env::temp_dir().join("ib-spread-trader-engine-fixtures");
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join(format!("{name}.sqlite3"));
        let _ = std::fs::remove_file(&file);

        let conn = rusqlite::Connection::open(&file).unwrap();
        conn.pragma_update(None, "user_version", 6i64).unwrap();
        conn.execute_batch(
            "CREATE TABLE contracts (id INTEGER PRIMARY KEY, symbol TEXT, kind TEXT,
                 exchange TEXT, currency TEXT, expiry TEXT, strike REAL, right TEXT);
             CREATE TABLE bars (contract_id INTEGER, ts TEXT, bar_size TEXT, data_type TEXT,
                 open REAL, high REAL, low REAL, close REAL, volume REAL);
             INSERT INTO contracts (id, symbol, kind, exchange, currency, expiry, strike, right)
             VALUES (1, 'TEST', 'stock', 'SMART', 'USD', NULL, NULL, NULL);",
        )
        .unwrap();

        let strikes: Vec<f64> = (35..=65).map(|i| i as f64 * 2.0).collect();
        let expiry_text = expiry().format("%Y-%m-%d").to_string();
        let mut next_id = 2i64;
        let mut ids = Vec::new();
        for &strike in &strikes {
            for right in ["C", "P"] {
                conn.execute(
                    "INSERT INTO contracts (id, symbol, kind, exchange, currency, expiry, strike, right)
                     VALUES (?1, 'TEST', 'option', 'SMART', 'USD', ?2, ?3, ?4)",
                    rusqlite::params![next_id, expiry_text, strike, right],
                )
                .unwrap();
                ids.push((next_id, strike, right == "C"));
                next_id += 1;
            }
        }

        let start = Utc.with_ymd_and_hms(2026, 7, 1, 13, 30, 0).unwrap();
        let expiry_instant = pricing::expiry_instant(expiry()).unwrap();
        conn.execute_batch("BEGIN").unwrap();
        let mut insert = conn
            .prepare(
                "INSERT INTO bars (contract_id, ts, bar_size, data_type, open, high, low, close, volume)
                 VALUES (?1, ?2, '30min', ?3, ?4, ?4, ?4, ?4, 0)",
            )
            .unwrap();

        for (bar, &spot) in path.iter().enumerate() {
            let ts = (start + Duration::minutes(30 * bar as i64))
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, false);
            for (kind, price) in [("trades", spot), ("bid", spot - 0.01), ("ask", spot + 0.01)] {
                insert
                    .execute(rusqlite::params![1i64, &ts, kind, price])
                    .unwrap();
            }
            let t = pricing::year_fraction(
                start + Duration::minutes(30 * bar as i64),
                expiry_instant,
            );
            for &(id, strike, is_call) in &ids {
                // After the dropout bar, skip strikes within ±20 of spot.
                // The wings keep quoting so the chain still has timestamps.
                if bar >= dropout_after && (strike - spot).abs() < 20.0 {
                    continue;
                }
                let mid = pricing::black76(spot, strike, t, VOL, is_call).max(0.06);
                insert
                    .execute(rusqlite::params![id, &ts, "bid", mid - 0.05])
                    .unwrap();
                insert
                    .execute(rusqlite::params![id, &ts, "ask", mid + 0.05])
                    .unwrap();
            }
        }

        let settle_ts = Utc
            .with_ymd_and_hms(EXPIRY.0, EXPIRY.1, EXPIRY.2, 19, 30, 0)
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, false);
        insert
            .execute(rusqlite::params![1i64, &settle_ts, "trades", settlement])
            .unwrap();

        drop(insert);
        conn.execute_batch("COMMIT").unwrap();
        drop(conn);
        file
    }

    pub fn series(name: &str, path: &[f64], settlement: f64) -> (Snapshot, ChainSeries) {
        let file = snapshot_with(name, path, settlement);
        let snapshot = Snapshot::open(&file).unwrap();
        let series = ChainSeries::load(&snapshot, "TEST", expiry()).unwrap();
        (snapshot, series)
    }

    pub fn bull_call_strategy() -> Strategy {
        use crate::execution::DeltaSelector;
        use crate::signal::RsiCross;
        use crate::structure::Vertical;
        Strategy {
            name: "rsi-bull-call".into(),
            signal: Box::new(RsiCross::new(14, 30.0).unwrap()),
            structure: Box::new(Vertical::bull_call(0.50, 0.30, 1).unwrap()),
            selector: Box::new(DeltaSelector {
                delta_tolerance: 0.07,
                max_debit: None,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use crate::portfolio::ExitReason;
    use crate::signal::{MarketView, Signal, Trigger};

    fn config() -> RunConfig {
        RunConfig {
            exits: ExitRules {
                profit_target_pct: Some(50.0),
                stop_loss_pct: Some(100.0),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// The fixture has to be trustworthy before anything is concluded from it:
    /// the parity fit must recover the path the options were priced off.
    #[test]
    fn the_fixture_chain_reconstructs_its_own_underlying() {
        let path = v_shaped_path();
        let (_snap, series) = series("selfcheck", &path, 120.0);
        assert_eq!(series.len(), path.len());

        for index in [0, 20, 50, path.len() - 1] {
            let bar = series.snapshot(index).unwrap();
            assert!(
                (bar.forward - path[index]).abs() < 0.05,
                "bar {index}: forward {} vs spot {}",
                bar.forward,
                path[index]
            );
            assert!((bar.reference_price() - path[index]).abs() < 0.05);
        }
    }

    #[test]
    fn a_recovery_opens_a_bull_call_and_the_run_accounts_for_it() {
        let (_snap, series) = series("basic", &v_shaped_path(), 120.0);
        let mut strategy = bull_call_strategy();
        let run = run_symbol(&series, &mut strategy, &config()).unwrap();

        assert_eq!(run.census.triggers, 1, "one crossing of 30");
        assert_eq!(
            run.census.total_rejections(),
            0,
            "a dense ladder rejects nothing"
        );
        assert_eq!(run.portfolio.closed.len(), 1);
        assert_eq!(run.equity_curve.len(), series.len());

        let trade = &run.portfolio.closed[0];
        assert_eq!(trade.symbol, "TEST");
        assert!(trade.entry_debit > 0.0, "a bull call is a debit");
        assert!(trade.commission > 0.0);
    }

    /// Cash, the trade log and the equity curve must agree, or the whole report
    /// is decoration.
    #[test]
    fn final_equity_equals_initial_cash_plus_every_trades_pnl() {
        let (_snap, series) = series("accounting", &v_shaped_path(), 120.0);
        let mut strategy = bull_call_strategy();
        let run = run_symbol(&series, &mut strategy, &config()).unwrap();

        let booked: f64 = run.portfolio.closed.iter().map(|t| t.pnl).sum();
        assert!(
            run.portfolio.open.is_empty(),
            "everything closed or settled"
        );
        assert!(
            (run.final_equity - (run.initial_cash + booked)).abs() < 1e-9,
            "{} vs {}",
            run.final_equity,
            run.initial_cash + booked
        );
    }

    /// What the `SameBar` option is actually for. It is look-ahead — the fill
    /// happens at the bar whose close produced the signal — but look-ahead is an
    /// unfair advantage in expectation, **not** a guarantee on any single path,
    /// so asserting it wins would be asserting something untrue. What must hold
    /// is that the timing model is wired in at all: the same signal opens the
    /// same trade exactly one bar apart, at a different price.
    #[test]
    fn next_bar_timing_delays_the_fill_by_exactly_one_bar() {
        let (_snap, series) = series("timing", &v_shaped_path(), 120.0);

        let mut next_bar = bull_call_strategy();
        let honest = run_symbol(&series, &mut next_bar, &config()).unwrap();

        let mut same_bar = bull_call_strategy();
        let flattered = run_symbol(
            &series,
            &mut same_bar,
            &RunConfig {
                timing: Timing::SameBar,
                ..config()
            },
        )
        .unwrap();

        let (a, b) = (&honest.portfolio.closed[0], &flattered.portfolio.closed[0]);
        assert_eq!(
            a.opened_at - b.opened_at,
            chrono::Duration::minutes(30),
            "next-bar should open one 30-minute bar later"
        );
        assert_ne!(
            a.entry_debit, b.entry_debit,
            "and therefore at a different price"
        );
    }

    /// A strategy that never trades must return exactly what it started with —
    /// no drift, no accidental commission, no marks left over.
    #[test]
    fn a_strategy_that_never_fires_returns_the_initial_cash_exactly() {
        struct Silent;
        impl Signal for Silent {
            fn name(&self) -> &str {
                "silent"
            }
            fn evaluate(&mut self, _: &MarketView<'_>) -> Option<Trigger> {
                None
            }
        }

        let (_snap, series) = series("flat", &v_shaped_path(), 120.0);
        let mut strategy = bull_call_strategy();
        strategy.signal = Box::new(Silent);
        let run = run_symbol(&series, &mut strategy, &config()).unwrap();

        assert_eq!(run.final_equity, run.initial_cash);
        assert_eq!(run.census.triggers, 0);
        assert!(run.portfolio.closed.is_empty());
        assert!(run.equity_curve.iter().all(|&(_, e)| e == run.initial_cash));
    }

    /// With no exit rules at all, the position has to survive to expiry and
    /// settle at intrinsic — the path the option tape's early stop forces.
    #[test]
    fn a_position_with_no_exits_settles_at_intrinsic() {
        let (_snap, series) = series("settle", &v_shaped_path(), 120.0);
        let mut strategy = bull_call_strategy();
        let run = run_symbol(
            &series,
            &mut strategy,
            &RunConfig {
                exits: ExitRules::default(),
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(run.settlement_price, Some(120.0));
        let trade = run.portfolio.closed.first().expect("one position settled");
        assert_eq!(trade.exit_reason, ExitReason::Expiry);

        // The curve must end where the summary says it ended, at the settlement
        // instant — not at the final option bar's mark a day earlier.
        let &(last_ts, last_equity) = run.equity_curve.last().unwrap();
        assert_eq!(last_ts, series.expiry_instant());
        assert_eq!(last_equity, run.final_equity);

        let (traded, settled) = run.portfolio.realized_split();
        assert_eq!(traded, 0.0, "nothing closed on a quote");
        assert!(settled != 0.0);

        // Settling at 120 puts both legs of any bull call deep in the money, so
        // the spread is worth its full width.
        let spread = &trade.description;
        assert!(spread.contains("TEST"), "{spread}");
        assert!(trade.exit_credit > 0.0);
    }

    /// The position cap is what keeps a repeatedly-firing signal from stacking.
    #[test]
    fn the_position_cap_is_enforced_and_counted() {
        struct Always;
        impl Signal for Always {
            fn name(&self) -> &str {
                "always"
            }
            fn evaluate(&mut self, view: &MarketView<'_>) -> Option<Trigger> {
                Some(Trigger {
                    ts: view.chain.ts,
                    direction: crate::signal::Direction::Bullish,
                    strength: 1.0,
                })
            }
        }

        let (_snap, series) = series("cap", &v_shaped_path(), 120.0);
        let mut strategy = bull_call_strategy();
        strategy.signal = Box::new(Always);
        let run = run_symbol(
            &series,
            &mut strategy,
            &RunConfig {
                exits: ExitRules::default(),
                ..Default::default()
            },
        )
        .unwrap();

        assert!(run.census.triggers > 50, "fired on nearly every bar");
        assert!(
            run.census.at_position_limit > 0,
            "and was refused after the first"
        );
        assert_eq!(
            run.portfolio.closed.len(),
            1,
            "exactly one position ever opened"
        );
    }

    /// A structure whose right disagrees with the trigger is declined, and that
    /// is counted apart from a selector rejection — they mean different things.
    #[test]
    fn a_declined_trigger_is_counted_separately_from_a_rejection() {
        use crate::structure::Vertical;
        let (_snap, series) = series("declined", &v_shaped_path(), 120.0);
        let mut strategy = bull_call_strategy();
        // Bear put against a bullish RSI recovery: never agrees.
        strategy.structure = Box::new(Vertical::bear_put(0.50, 0.30, 1).unwrap());
        let run = run_symbol(&series, &mut strategy, &config()).unwrap();

        assert_eq!(run.census.triggers, 1);
        assert_eq!(run.census.declined, 1);
        assert_eq!(run.census.total_rejections(), 0);
        assert!(run.portfolio.closed.is_empty());
        assert_eq!(run.final_equity, run.initial_cash);
    }

    /// A ladder that cannot supply the asked-for delta must reject, and the
    /// census must say what it did have.
    #[test]
    fn an_unreachable_delta_target_is_rejected_with_a_readable_example() {
        // The fixture's ladder is dense — $2 strikes — so adjacent deltas are
        // still points apart. A tolerance this tight cannot be met by anything,
        // which is the same shape as META's ladder simply stopping short.
        use crate::execution::DeltaSelector;
        let (_snap, series) = series("unreachable", &v_shaped_path(), 120.0);
        let mut strategy = bull_call_strategy();
        strategy.selector = Box::new(DeltaSelector {
            delta_tolerance: 1e-6,
            max_debit: None,
        });
        let run = run_symbol(&series, &mut strategy, &config()).unwrap();

        assert_eq!(run.census.triggers, 1);
        assert_eq!(run.census.total_rejections(), 1);
        let (kind, example) = run.census.rejection_examples.iter().next().unwrap();
        assert_eq!(kind, "no_strike_near_target_delta");
        assert!(example.contains("closest quoted"), "{example}");
    }

    /// When option quotes disappear but a time-based exit rule fires, the
    /// engine closes the position at intrinsic rather than letting it ride to
    /// expiry settlement.
    #[test]
    fn unquoted_legs_close_at_intrinsic_when_a_time_rule_fires() {
        let path = v_shaped_path(); // 81 bars, ~1.7 days from Jul 1
        // ATM-ish strikes (±20 of spot) go quiet after bar 40. The position
        // opens around bar 31. A very generous min_days_before_expiry fires
        // once quotes are unavailable on the held strikes.
        let file = snapshot_with_dropout("dropout", &path, 120.0, 40);
        let snapshot = crate::data::Snapshot::open(&file).unwrap();
        let series = ChainSeries::load(&snapshot, "TEST", expiry()).unwrap();

        let mut strategy = bull_call_strategy();
        // min_days_before_expiry = 50: fires when remaining < 50 days.
        // The fixture starts ~51 days before expiry. Remaining crosses below 50
        // at bar ~62 (Jul 2 20:30) — well after bar 40 where ATM quotes vanish,
        // so the exit fires via time_triggered, not triggered.
        let run = run_symbol(
            &series,
            &mut strategy,
            &RunConfig {
                exits: ExitRules {
                    min_days_before_expiry: Some(50),
                    stop_loss_pct: Some(100.0),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(run.census.triggers, 1, "one RSI crossing");
        assert_eq!(
            run.census.synthetic_exits, 1,
            "closed at intrinsic when quotes vanished"
        );
        assert_eq!(run.portfolio.closed.len(), 1);

        let trade = &run.portfolio.closed[0];
        assert!(trade.synthetic_price, "flagged as synthetic");
        assert_eq!(trade.exit_reason, ExitReason::BeforeExpiry);
    }
}
