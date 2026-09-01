//! Golden tests against the real capture snapshot.
//!
//! These pin values confirmed by hand, so a refactor that quietly changes the
//! maths fails loudly rather than drifting. They **skip cleanly when no snapshot
//! is present** — following `~/ib-option-viz/tests/test_golden.py`, since the
//! snapshot is a 213 MB file that lives outside the repo and cannot be checked in.
//!
//! Set `$IB_SNAPSHOT` to test against a specific file; otherwise the newest in
//! `~/Documents/Options Database` is used.
//!
//! When one of these fails, decide which happened before touching it:
//! a regression, or a deliberate change to the model. Only the second is a
//! reason to edit the expected value, and the commit should say so.

use chrono::{DateTime, NaiveDate, Utc};
use ib_spread_trader::config::StrategyConfig;
use ib_spread_trader::data::{ChainSeries, Snapshot, chain_coverage};
use ib_spread_trader::domain::OptionRight;
use ib_spread_trader::{engine, report};

/// The bar every hand-checked value below was taken at.
const PINNED_TS: &str = "2026-08-19T15:00:00+00:00";
const EXPIRY: (i32, u32, u32) = (2026, 8, 21);

fn expiry() -> NaiveDate {
    NaiveDate::from_ymd_opt(EXPIRY.0, EXPIRY.1, EXPIRY.2).unwrap()
}

fn pinned_ts() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(PINNED_TS)
        .unwrap()
        .with_timezone(&Utc)
}

/// `None` means "no snapshot here" — the caller returns and the test passes.
fn snapshot() -> Option<Snapshot> {
    let path = match std::env::var_os("IB_SNAPSHOT") {
        Some(path) => std::path::PathBuf::from(path),
        None => ib_spread_trader::data::snapshot::default_snapshot_path().ok()?,
    };
    match Snapshot::open(&path) {
        Ok(snapshot) => Some(snapshot),
        Err(err) => {
            eprintln!("skipping golden test: {err}");
            None
        }
    }
}

macro_rules! snapshot_or_skip {
    () => {
        match snapshot() {
            Some(snapshot) => snapshot,
            None => {
                eprintln!("skipping: no capture snapshot available");
                return;
            }
        }
    };
}

/// The shape of the reference capture. If this changes, a *different* snapshot
/// is being tested and every other value below should be re-confirmed by hand
/// rather than adjusted to fit.
#[test]
fn the_catalog_reports_one_expiry_across_twelve_symbols() {
    let snapshot = snapshot_or_skip!();
    let chains = chain_coverage(&snapshot).unwrap();

    assert_eq!(chains.len(), 12, "expected 12 tradeable chains");
    assert!(
        chains.iter().all(|c| c.expiry == "2026-08-21"),
        "the reference snapshot has option bars on exactly one expiry"
    );
    assert!(chains.iter().all(|c| c.is_tradeable()));

    let spy = chains.iter().find(|c| c.symbol == "SPY").unwrap();
    assert_eq!(spy.timestamps, 581);
    assert_eq!(spy.paired_strikes, 45);
    // SPY options trade to 20:00 UTC, past the equity close, so ~7% of their
    // bars have no stock print to join.
    assert_eq!(spy.stock_ts_overlap, 539);
    assert!(spy.stock_ts_overlap < spy.timestamps);

    // AAPL, AMZN, COIN and NVDA quote strikes on one side only.
    let amzn = chains.iter().find(|c| c.symbol == "AMZN").unwrap();
    assert_eq!((amzn.strikes, amzn.paired_strikes), (36, 24));
}

/// The number that validates the whole pricing stack: the forward recovered from
/// 30 strikes by put-call parity, against the underlying's own printed mid.
#[test]
fn spys_parity_forward_matches_its_real_spot_to_the_carry_basis() {
    let snapshot = snapshot_or_skip!();
    let series = ChainSeries::load(&snapshot, "SPY", expiry()).unwrap();
    let bar = series
        .snapshot_at(pinned_ts())
        .expect("the pinned bar exists");

    assert_eq!(bar.paired_strikes, 30);
    assert!((bar.forward - 771.405).abs() < 1e-9, "{}", bar.forward);
    assert!(
        (bar.residual_mad - 0.02).abs() < 1e-9,
        "{}",
        bar.residual_mad
    );

    let spot = bar
        .underlying
        .mid
        .expect("a mid-session bar has a stock print");
    assert!((spot - 771.12).abs() < 1e-9, "{spot}");
    assert_eq!(bar.underlying.open, Some(772.20));
    assert_eq!(bar.underlying.high, Some(772.47));
    assert_eq!(bar.underlying.low, Some(770.88));
    assert_eq!(bar.underlying.close, Some(771.11));
    assert_eq!(bar.underlying.volume, Some(33_973.0));

    // +0.037%: the right sign and size for carry, and the reason the forward is
    // a sound fallback on the 20:00 bars that have no spot at all.
    let basis = 100.0 * (bar.forward / spot - 1.0);
    assert!((0.02..0.06).contains(&basis), "carry basis was {basis}%");
}

/// Call and put IV at the same strike are computed from different prices, so
/// their agreement is an independent check on the inversion.
#[test]
fn call_and_put_implied_vol_agree_at_the_money() {
    let snapshot = snapshot_or_skip!();
    let series = ChainSeries::load(&snapshot, "SPY", expiry()).unwrap();
    let bar = series.snapshot_at(pinned_ts()).unwrap();

    let at = |right| {
        bar.quotes
            .iter()
            .find(|q| q.strike == 771.0 && q.right == right)
            .and_then(|q| q.implied_vol)
            .expect("strike 771 is quoted on both sides")
    };
    let (call, put) = (at(OptionRight::Call), at(OptionRight::Put));
    assert!((call - 0.1071).abs() < 5e-5, "{call}");
    assert!((put - 0.1067).abs() < 5e-5, "{put}");
    assert!((call - put).abs() < 0.001, "{call} vs {put}");

    // Exact value generated by ib-option-viz's OTM-smile interpolation.
    let atm = bar.atm_implied_vol().unwrap();
    assert!((atm - 0.105_975_523_476_461_76).abs() < 1e-12, "{atm}");
}

/// Deep-ITM calls quoting at or below intrinsic have no solvable vol. They are
/// *unusable*, not zero-delta, and must stay out of the selectable set — this is
/// the guard that keeps a delta selector from picking a strike whose distance
/// from the money is unknown.
#[test]
fn deep_itm_calls_have_no_delta_and_are_not_selectable() {
    let snapshot = snapshot_or_skip!();
    let series = ChainSeries::load(&snapshot, "SPY", expiry()).unwrap();
    let bar = series.snapshot_at(pinned_ts()).unwrap();

    for strike in [719.0, 723.0, 748.0, 751.0] {
        let quote = bar
            .quotes
            .iter()
            .find(|q| q.strike == strike && q.right == OptionRight::Call)
            .unwrap_or_else(|| panic!("strike {strike} is quoted"));
        assert_eq!(quote.implied_vol, None, "strike {strike} should not solve");
        assert_eq!(quote.delta, None);
    }

    let selectable: Vec<f64> = bar
        .selectable(OptionRight::Call)
        .map(|q| q.strike)
        .collect();
    assert!(!selectable.contains(&719.0));
    assert!(selectable.contains(&771.0), "the money is still selectable");
}

/// The tape's own defects, pinned so a change in the cleaning rules shows up.
#[test]
fn spys_quality_account_matches_the_capture() {
    let snapshot = snapshot_or_skip!();
    let series = ChainSeries::load(&snapshot, "SPY", expiry()).unwrap();

    assert_eq!(series.len(), 581);
    assert_eq!(series.option_quality.rows_read, 48_660);
    assert_eq!(series.option_quality.dropped_crossed, 11);
    assert_eq!(series.option_quality.dropped_unpaired, 0);
    assert_eq!(series.option_quality.dropped_nonpositive, 0);

    // The stock tape is spotless on every symbol.
    // Underlying quality tracks the signal resolution: 5-min when available.
    assert_eq!(series.underlying_quality.rows_read, 4836);
    assert_eq!(series.underlying_quality.rows_kept(), 4836);
}

/// The option tape stops the day before expiry, so settlement depends on a stock
/// print that lands after the last option bar.
#[test]
fn the_underlying_still_prints_after_the_last_option_bar() {
    let snapshot = snapshot_or_skip!();
    let series = ChainSeries::load(&snapshot, "SPY", expiry()).unwrap();

    let last_option_bar = series.timestamps().last().unwrap();
    assert_eq!(last_option_bar.to_rfc3339(), "2026-08-20T20:00:00+00:00");
    assert!(last_option_bar < series.expiry_instant());

    let settlement = series.underlying_asof(series.expiry_instant()).unwrap();
    assert!(
        settlement.price().is_some(),
        "expiry-day stock print exists"
    );
}

/// The driftless model — the honest random-walk null — pinned end to end. With
/// no directional view the only edge is theta, so it trades almost entirely
/// credit spreads. These numbers are deterministic; a change means the model or
/// its config changed, intentionally or otherwise.
#[test]
fn the_driftless_model_reproduces_its_recorded_run() {
    let snapshot = snapshot_or_skip!();
    let config = StrategyConfig::load("strategies/driftless.toml").unwrap();
    let run_config = config.run_config();

    let mut runs = Vec::new();
    for symbol in &config.symbols {
        let series = ChainSeries::load(&snapshot, symbol, config.expiry).unwrap();
        let mut strategy = config.strategy().unwrap();
        runs.push(engine::run_symbol(&series, &mut strategy, &run_config).unwrap());
    }
    let built = report::build(&config.name, config.expiry, &runs);
    let totals = &built.totals;

    assert_eq!(totals.symbols, 12);
    // Credit spreads are held the full forecast window now (the short-strike exit
    // no longer closes them early), so the single position slot turns over slowly:
    // 4 trades, all credit, all closing on the forecast horizon — and holding them
    // full is what makes them a profitable theta harvest.
    assert_eq!(totals.trades, 4);

    let funnel = &totals.census.ranking;
    assert_eq!(funnel.enumerated, 7_892_475);
    assert_eq!(funnel.priceable, 7_892_475);
    assert_eq!(funnel.liquid, 7_701_882);
    assert_eq!(funnel.within_loss, 2_158_733);
    assert_eq!(funnel.positive_ev, 1_292);
    assert_eq!(funnel.above_min_score, 20);
    assert_eq!(funnel.chosen, 4, "chosen must equal the trade count");

    // A random walk has no directional edge (~49% hit-rate), so it neither beats
    // nor clearly loses to the null on CRPS.
    let pooled = &built.model_eval.as_ref().unwrap().pooled;
    assert!(pooled.crps_skill.abs() < 0.01, "skill {}", pooled.crps_skill);

    assert!((totals.total_pnl - 385.60).abs() < 0.01, "{}", totals.total_pnl);
    assert!(totals.settled_pnl.abs() < 0.01, "{}", totals.settled_pnl);
    // Every credit spread runs to the end of its forecast window.
    assert_eq!(totals.exits_by_reason["forecast_horizon"], 4);
    assert!(built.trades.iter().all(|t| t.forecast_target.is_some()));

    // All four picks are credit spreads (the driftless model's theta harvest).
    let credits = built.trades.iter().filter(|t| t.entry_debit < 0.0).count();
    assert_eq!(credits, 4);

    // Cash, the trade log and the curve must agree.
    let booked: f64 = built.trades.iter().map(|t| t.pnl).sum();
    assert!((totals.total_pnl - booked).abs() < 1e-6);
    let curve = report::combined_curve(&runs);
    assert!((curve.last().unwrap().equity - totals.final_equity).abs() < 1e-6);
}

/// The mean-reversion model — the main strategy — pinned end to end. The
/// directional forecast (fade the trend) beats the random-walk null, and that
/// unlocks the structure mix: debit spreads and naked longs instead of the
/// driftless model's credit-only theta harvest.
#[test]
fn the_mean_reversion_model_reproduces_its_recorded_run() {
    let snapshot = snapshot_or_skip!();
    let config = StrategyConfig::load("strategies/mean_reversion.toml").unwrap();
    let run_config = config.run_config();

    let mut runs = Vec::new();
    for symbol in &config.symbols {
        let series = ChainSeries::load(&snapshot, symbol, config.expiry).unwrap();
        let mut strategy = config.strategy().unwrap();
        runs.push(engine::run_symbol(&series, &mut strategy, &run_config).unwrap());
    }
    let built = report::build(&config.name, config.expiry, &runs);
    let totals = &built.totals;

    assert_eq!(totals.symbols, 12);
    assert_eq!(totals.trades, 51);

    let funnel = &totals.census.ranking;
    assert_eq!(funnel.enumerated, 3_283_433);
    assert_eq!(funnel.chosen, 51);

    // The forecast beats the null: positive CRPS skill and a >50% directional
    // hit-rate. This is the whole reason the mean-reversion model exists.
    let pooled = &built.model_eval.as_ref().unwrap().pooled;
    assert!(pooled.crps_skill > 0.0, "skill {}", pooled.crps_skill);
    assert!(
        pooled.directional_hit_rate > 0.5,
        "dir {}",
        pooled.directional_hit_rate
    );

    // The directional forecast routes selection into a diversified mix — debit
    // spreads (34) and naked longs (12) that the driftless credit harvester never
    // picks, plus some credit spreads (5) held their full window. All three
    // structures trade. (P&L on ~51 trades is noise; the forecast is the validated
    // part.) A naked long is a single leg — no short strike, so no '-' in the label.
    let naked = built.trades.iter().filter(|t| !t.description.contains('-')).count();
    let debit = built
        .trades
        .iter()
        .filter(|t| t.entry_debit > 0.0 && t.description.contains('-'))
        .count();
    let credit = built.trades.iter().filter(|t| t.entry_debit < 0.0).count();
    assert!(naked > 0 && debit > 0, "naked {naked} debit {debit}");
    assert!(debit > naked && debit > credit, "debit spreads dominate");
    // Most positions run to the end of their forecast window.
    assert!(totals.exits_by_reason["forecast_horizon"] > totals.trades / 2);
    assert!((totals.total_pnl - -1141.00).abs() < 0.01, "{}", totals.total_pnl);
    assert!(totals.settled_pnl.abs() < 0.01, "{}", totals.settled_pnl);

    let booked: f64 = built.trades.iter().map(|t| t.pnl).sum();
    assert!((totals.total_pnl - booked).abs() < 1e-6);
}

/// The shared account runs all twelve symbols against one $20k pool over a
/// merged timeline: entries compete for the capital, funded best-score-first and
/// sized against what is free, capped at one position per symbol. This pins the
/// account result and the two invariants that make it a real limited-capital run
/// rather than the sum of independent per-symbol accounts.
#[test]
fn the_shared_account_runs_all_symbols_against_one_pool() {
    let snapshot = snapshot_or_skip!();
    let config = StrategyConfig::load("strategies/shared_20k.toml").unwrap();
    let run_config = config.run_config();

    // One shared account: the per-symbol cap defaults to 1 in shared mode.
    assert_eq!(run_config.max_positions_per_symbol, Some(1));

    let mut all_series = Vec::new();
    for symbol in &config.symbols {
        let series = ChainSeries::load(&snapshot, symbol, config.expiry).unwrap();
        if !series.is_empty() {
            all_series.push(series);
        }
    }
    let mut strategies: Vec<_> = all_series
        .iter()
        .map(|_| config.strategy().unwrap())
        .collect();
    let shared = engine::run_shared(&all_series, &mut strategies, &run_config).unwrap();
    let built = report::build_shared(&config.name, config.expiry, &shared);
    let totals = &built.totals;

    // One $20k pool, not $20k per symbol. This is the mean-reversion model under
    // a shared account with a risk cap (10%/pos, 25% max) and a lot cap (5). The
    // directional model carries naked longs, so the curve is more volatile than
    // the driftless credit harvester — the sizing caps bound it, not smooth it.
    assert!((totals.capital_deployed - 20_000.0).abs() < 1e-6);
    assert_eq!(totals.trades, 51);
    assert!((totals.total_pnl - -5510.10).abs() < 0.01, "{}", totals.total_pnl);
    assert!(totals.settled_pnl.abs() < 0.01, "{}", totals.settled_pnl);
    // The lot cap binds before capital, so the account never fills and no
    // opportunity is turned away for lack of capital.
    assert_eq!(totals.census.capital_limited, 0);

    // Invariant 1: capital at risk never exceeds the one $20k pool; invariant 2:
    // at most one position is open per symbol at any instant; invariant 3: the
    // lot cap bounds every fill. Sweep the trade open/close events in time order.
    use std::collections::BTreeMap;
    // First leg's contract count from e.g. "QQQ +5C 743 -5C 738".
    let lots = |desc: &str| -> u32 {
        for tok in desc.split_whitespace() {
            if let Some(rest) = tok.strip_prefix('+') {
                let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                if let Ok(n) = digits.parse::<u32>() {
                    return n;
                }
            }
        }
        1
    };
    for t in &built.trades {
        assert!(lots(&t.description) <= 5, "{} exceeds the 5-lot cap", t.description);
    }
    let mut events: Vec<(chrono::DateTime<Utc>, f64, i64, &str)> = Vec::new();
    for t in &built.trades {
        let risk = t.max_loss.unwrap_or(t.entry_debit.max(0.0));
        events.push((t.opened_at, risk, 1, &t.symbol));
        events.push((t.closed_at, -risk, -1, &t.symbol));
    }
    events.sort_by_key(|e| e.0);
    let (mut risk, mut peak_risk) = (0.0_f64, 0.0_f64);
    let mut per_symbol: BTreeMap<&str, i64> = BTreeMap::new();
    for (_, dr, dn, sym) in events {
        risk += dr;
        peak_risk = peak_risk.max(risk);
        let count = per_symbol.entry(sym).or_default();
        *count += dn;
        assert!(*count <= 1, "more than one {sym} position open at once");
    }
    assert!(peak_risk <= 20_000.0, "peak risk {peak_risk} exceeded the pool");

    // Cash, the trade log and the single account curve must agree.
    let booked: f64 = built.trades.iter().map(|t| t.pnl).sum();
    assert!((totals.total_pnl - booked).abs() < 1e-6);
    assert!((shared.equity_curve.last().unwrap().equity - totals.final_equity).abs() < 1e-6);
}

/// A strategy that never fires must return exactly its capital — no drift, no
/// stray commission — even over the full real chain.
#[test]
fn a_never_firing_strategy_returns_its_capital_exactly_on_real_data() {
    let snapshot = snapshot_or_skip!();
    // An RSI level of 0 is unreachable, so the signal cannot cross it.
    let config: StrategyConfig = toml::from_str(
        "symbols = [\"SPY\"]\nexpiry = \"2026-08-21\"\n[signal]\nkind = \"rsi_cross\"\nlevel = 0.0",
    )
    .unwrap();
    let series = ChainSeries::load(&snapshot, "SPY", config.expiry).unwrap();
    let mut strategy = config.strategy().unwrap();
    let run = engine::run_symbol(&series, &mut strategy, &config.run_config()).unwrap();

    assert_eq!(run.census.triggers, 0);
    assert_eq!(run.final_equity, run.initial_cash);
    assert!(run.equity_curve.iter().all(|p| p.equity == run.initial_cash));
}
