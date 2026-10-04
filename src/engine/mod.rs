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
use chrono::{DateTime, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::data::{ChainSeries, ChainSnapshot, EarningsEvent, Quality};
use crate::execution::{Costs, Reject, Selector};
use crate::model::{
    Candidate, CandidateConfig, EarningsDistance, FeatureEngine, Features, ForecastRecord,
    PriceDistribution, PriceModel, RankingConfig, RankingFunnel, ScorerRecord, choice_key,
    enumerate_verticals, rank, ranking_key,
};
use crate::portfolio::{
    AllocationCandidate, ExitReason, OpenPosition, Portfolio, PortfolioModel, PortfolioModelKind,
    PortfolioState, TradePrediction,
};
use crate::signal::{MarketView, Signal};
use crate::spread::Spread;
use crate::structure::Structure;

/// Seconds in a calendar year, matching `pricing::year_fraction`.
const SECONDS_PER_YEAR: f64 = 365.25 * 24.0 * 3600.0;

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
    /// How many spreads may be open at once **across the whole account**. One
    /// means the strategy is simply in or out; without a cap a repeatedly-firing
    /// signal stacks positions.
    pub max_open_positions: usize,
    /// How many positions may be open **in a single symbol** at once. `None`
    /// leaves the account-wide cap the only limit; the shared account defaults
    /// this to 1 so capital diversifies across names rather than piling into one.
    pub max_positions_per_symbol: Option<usize>,
    /// Refuse new entries when fewer than this many calendar days remain before
    /// expiry. Prevents opening positions the before-expiry exit would
    /// immediately close.
    pub min_entry_days_before_expiry: Option<u32>,
    /// Which open chain a new entry may use when a symbol has several.
    pub expiry_choice: ExpiryChoice,
    /// Refuse new entries within this many calendar days of an earnings
    /// announcement. Prevents the same open-close churn as the expiry gate.
    pub earnings_blackout_days: Option<u32>,
    /// Entry-timing gate on RSI of the option-bar spot series. A bar outside
    /// `[min_entry_rsi, max_entry_rsi]` opens nothing.
    ///
    /// This gates on *where the model has skill*, not on where price is going:
    /// on the reference snapshot the forecast beats its null by +0.032 when
    /// oversold and loses to it when overbought. It is therefore a claim about
    /// this model in this sample, and the sample rose — in a falling market a
    /// mean-reversion forecaster would plausibly be broken at the other end.
    /// Ranked mode only; the staged path has no feature engine to read.
    pub min_entry_rsi: Option<f64>,
    pub max_entry_rsi: Option<f64>,
    pub entry_rsi_zone: RsiZone,
    /// Refuse entries unless RSI crossed up through its level within this many
    /// bars — "recovering" rather than "oversold".
    pub max_bars_since_rsi_cross_up: Option<u32>,
    /// Refuse a symbol's entry until this many days after its last fill — the
    /// spacing that turns a per-symbol cap above one into a ladder.
    pub min_days_between_entries: Option<f64>,
    /// Decide entries only on bars starting inside this New York window, as
    /// minutes past midnight.
    pub entry_window: Option<(u16, u16)>,
    /// Ranked allocation policy. `None` preserves fixed one-lot entries; shared
    /// ranked configs resolve to the score-weighted model by default.
    pub portfolio_model: Option<PortfolioModelKind>,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            initial_cash: 100_000.0,
            costs: Costs::default(),
            timing: Timing::NextBar,
            exits: ExitRules::default(),
            max_open_positions: 1,
            max_positions_per_symbol: None,
            min_entry_days_before_expiry: None,
            expiry_choice: ExpiryChoice::default(),
            earnings_blackout_days: None,
            min_entry_rsi: None,
            max_entry_rsi: None,
            entry_rsi_zone: RsiZone::default(),
            max_bars_since_rsi_cross_up: None,
            min_days_between_entries: None,
            entry_window: None,
            portfolio_model: None,
        }
    }
}

/// The staged `signal -> structure -> selector` pipeline. Signals are stateful,
/// so each symbol needs its own.
pub struct StagedStrategy {
    pub name: String,
    pub signal: Box<dyn Signal>,
    pub structure: Box<dyn Structure>,
    pub selector: Box<dyn Selector>,
}

/// The ranked price-target pipeline: a feature engine feeds a price model, whose
/// distribution ranks the verticals the chain can supply. Also stateful (the
/// feature engine carries indicator state), so each symbol needs its own.
pub struct RankedStrategy {
    pub name: String,
    pub features: FeatureEngine,
    pub model: Box<dyn PriceModel>,
    pub candidates: CandidateConfig,
    pub ranking: RankingConfig,
}

/// One symbol's decision engine, in one of two shapes. The bar loop, gates,
/// timing and settlement are shared; only how a bar becomes an entry differs.
pub enum Strategy {
    Staged(StagedStrategy),
    /// Boxed: the ranked strategy carries a feature engine's worth of indicator
    /// state, much larger than the staged stages' three trait objects.
    Ranked(Box<RankedStrategy>),
}

impl Strategy {
    pub fn name(&self) -> &str {
        match self {
            Strategy::Staged(s) => &s.name,
            Strategy::Ranked(r) => &r.name,
        }
    }

    /// Signal-internal gate counts. The ranked path reports its funnel through the
    /// census instead, so this is empty for it.
    fn signal_diagnostics(&self) -> BTreeMap<String, usize> {
        match self {
            Strategy::Staged(s) => s.signal.diagnostics(),
            Strategy::Ranked(_) => BTreeMap::new(),
        }
    }
}

/// One point on an account equity curve: total equity marked to market, and the
/// capital currently at risk in open positions (what is "invested" — cash paid
/// for debits, margin posted for credits, both captured by max loss).
#[derive(Debug, Clone, Copy, Serialize)]
pub struct EquityPoint {
    pub ts: DateTime<Utc>,
    pub equity: f64,
    pub invested: f64,
}

/// The universal entry gates, shared by both pipelines: a bar may be refused
/// because a position is already open, or it is too close to expiry or earnings.
enum EntryGate {
    Open,
    PositionLimit,
    TooCloseToExpiry,
    EarningsBlackout,
    RsiOutOfBand,
    NoRecentRsiCross,
}

/// Record every open position's mark on this bar. Runs after `update_peaks`, so
/// the peak it reports is the one the exit check will read on this same bar.
fn record_marks(
    portfolio: &Portfolio,
    bar: &ChainSnapshot,
    config: &RunConfig,
    out: &mut Vec<PositionMark>,
) {
    for p in &portfolio.open {
        if p.spread.symbol != bar.symbol || p.expiry != bar.expiry {
            continue;
        }
        let Ok(credit) = crate::execution::exit_credit(&p.spread, bar) else {
            continue; // unquoted on this bar: no mark rather than a made-up one
        };
        out.push(PositionMark {
            ts: bar.ts,
            position_id: p.id,
            symbol: p.spread.symbol.clone(),
            gross_pnl: credit - p.entry_debit,
            peak_value: p.peak_value,
            trailing_stop: config.exits.trailing_stop_level(p),
            underlying: bar.underlying.price(),
        });
    }
}

fn entry_gate(
    open_positions: usize,
    exiting: usize,
    config: &RunConfig,
    expiry_instant: DateTime<Utc>,
    bar_ts: DateTime<Utc>,
    next_earnings: Option<&EarningsEvent>,
    timing: EntryTiming,
) -> EntryGate {
    let EntryTiming { rsi, bars_since_rsi_cross_up } = timing;
    let open_after_exits = open_positions - exiting;
    if open_after_exits >= config.max_open_positions {
        EntryGate::PositionLimit
    } else if too_close_to_expiry(config, expiry_instant, bar_ts) {
        EntryGate::TooCloseToExpiry
    } else if config
        .earnings_blackout_days
        .is_some_and(|days| exits::in_earnings_blackout(bar_ts, next_earnings, days))
    {
        EntryGate::EarningsBlackout
    } else if config.max_bars_since_rsi_cross_up.is_some_and(|window| {
        // No crossing yet is a refusal, unlike a missing RSI reading: "it has not
        // turned up" is an established fact, not an absent measurement.
        bars_since_rsi_cross_up.is_none_or(|n| n > window)
    }) {
        EntryGate::NoRecentRsiCross
    } else if rsi.is_some_and(|v| config.entry_rsi_zone.refuses(v, config)) {
        // A missing reading cannot refuse: the gate asserts "RSI is outside the
        // band", and a warming indicator has not established that.
        EntryGate::RsiOutOfBand
    } else {
        EntryGate::Open
    }
}

/// Whether a bar starting at `ts` falls inside the configured New York entry
/// window (always, when there is none).
fn inside_entry_window(config: &RunConfig, ts: DateTime<Utc>) -> bool {
    config.entry_window.is_none_or(|(from, to)| {
        let local = ts.with_timezone(&chrono_tz::America::New_York);
        let minute = (chrono::Timelike::hour(&local) * 60 + chrono::Timelike::minute(&local)) as u16;
        (from..=to).contains(&minute)
    })
}

/// Whether a chain expiring at `expiry_instant` is too near to open a position in.
fn too_close_to_expiry(config: &RunConfig, expiry_instant: DateTime<Utc>, ts: DateTime<Utc>) -> bool {
    config
        .min_entry_days_before_expiry
        .is_some_and(|min_days| (expiry_instant - ts).num_days() < min_days as i64)
}

/// The feature readings the entry-timing gates consult. Grouped because they are
/// one thing — what the feature engine saw this bar — and because the staged
/// path has none of them, which `EntryTiming::default()` says plainly.
#[derive(Debug, Clone, Copy, Default)]
struct EntryTiming {
    rsi: Option<f64>,
    bars_since_rsi_cross_up: Option<u32>,
}

/// Which side of the RSI band opens trades.
///
/// `Inside` trades the middle and refuses the extremes; `Outside` is its mirror,
/// refusing the middle so entries wait for a stretched tape in either direction.
/// The same two bounds describe both — only the sense of the comparison differs.
/// Which chains a new entry may open in, among those outside the entry
/// blackout. `Front` ranks only the nearest, so a run trades the front month
/// and moves to the next one once the front enters its blackout; `Best` ranks
/// every open chain and takes the best candidate whatever its expiry.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExpiryChoice {
    #[default]
    Front,
    Best,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RsiZone {
    #[default]
    Inside,
    Outside,
}

impl RsiZone {
    /// Whether this reading is refused. A bound that is not configured cannot
    /// refuse anything, which is what makes a one-sided band meaningful.
    fn refuses(self, v: f64, config: &RunConfig) -> bool {
        let (lo, hi) = (config.min_entry_rsi, config.max_entry_rsi);
        match self {
            Self::Inside => lo.is_some_and(|lo| v < lo) || hi.is_some_and(|hi| v > hi),
            // The exact complement of `Inside`, bounds included, so the two
            // zones partition the axis and a reading is never open to both.
            // Inside the band on *every* configured side, so a one-sided
            // `Outside` band still refuses the half it names.
            Self::Outside => {
                lo.is_none_or(|lo| v >= lo)
                    && hi.is_none_or(|hi| v <= hi)
                    && (lo.is_some() || hi.is_some())
            }
        }
    }
}

/// One open position, marked on one bar: what it is worth, the peak it has
/// reached, and the level each *dynamic* exit would close it at.
///
/// Static bounds (strikes, the entry forecast target and stop) are already on the
/// trade and do not need repeating per bar. What cannot be reconstructed after
/// the fact is anything that moves with the position's own history — the peak,
/// and the trailing stop that rides it — so that is what this records.
///
/// Purely observational: nothing in the engine reads it back. A decision that
/// consumed its own audit trail would be a look-ahead risk.
#[derive(Debug, Clone, Serialize)]
pub struct PositionMark {
    pub ts: DateTime<Utc>,
    pub position_id: usize,
    pub symbol: String,
    pub gross_pnl: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peak_value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trailing_stop: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub underlying: Option<f64>,
}

/// The two prices the entry forecast set for a position: where it takes profit,
/// and where the forecast has been left behind. Carried together because they
/// are read from the same distribution at the same instant.
#[derive(Debug, Clone, Copy, Default)]
pub struct ForecastBounds {
    pub target: Option<f64>,
    pub stop: Option<f64>,
}

/// A decision taken on one bar, to be executed on the next.
#[derive(Debug, Clone)]
enum Intent {
    Open {
        spread: Spread,
        /// The chain to fill in. Only that chain's bar quotes the legs.
        expiry: NaiveDate,
        prediction: Option<TradePrediction>,
        bounds: ForecastBounds,
        /// Boxed: the payload is a diagnostic, and inlining it would make every
        /// `Intent` as large as the largest one.
        entry_features: Option<Box<crate::model::EntryFeatures>>,
        /// Present when allocation is deferred to fill time (the shared-account
        /// path): `spread` is one lot and `execute` scales it against the
        /// portfolio's capital then. `None` means `spread` is already sized.
        allocation: Option<AllocationHint>,
        /// The chosen candidate's per-leg entry vols, carried onto the position
        /// for the live-POP exit. Empty for the staged path.
        leg_vols: Vec<f64>,
    },
    Close {
        id: usize,
        reason: ExitReason,
    },
}

/// What `execute` needs to size a one-lot spread at fill time: the ranker's
/// score (drives the budget) and one lot's max loss (divides it into lots).
#[derive(Debug, Clone, Copy)]
struct AllocationHint {
    score: f64,
    one_lot_max_loss: f64,
    /// The bar's IV regime, carried to fill time so the policy sizes against the
    /// state the decision was made in, not the state at execution.
    iv_vs_median: Option<f64>,
    /// The one-lot expected P/L, scaled by the filled lot count for the trade
    /// prediction so it stays consistent with the position's realised size.
    expected_pl: f64,
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
    /// Refused because within the earnings blackout window.
    pub earnings_blackout: usize,
    /// Refused because RSI sat outside the configured entry band.
    pub rsi_out_of_band: usize,
    /// Refused because RSI had not recently crossed up through its level.
    pub no_recent_rsi_cross: usize,
    /// Refused because the symbol's last entry was too recent.
    pub entry_spacing: usize,
    /// Refused because the bar fell outside the entry window.
    pub outside_entry_window: usize,
    /// Closed at intrinsic because option quotes were unavailable.
    pub synthetic_exits: usize,
    /// A ranked choice was made but the capital left could not fund even one
    /// lot at its max loss. Only the sized ranked path counts this.
    pub capital_limited: usize,
    /// The ranked path's candidate funnel, accumulated across bars. Zeroed for
    /// the staged path, which reports through `triggers`/`declined`/`rejections`.
    pub ranking: RankingFunnel,
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
    pub equity_curve: Vec<EquityPoint>,
    /// The underlying print settlement used, if anything was still open.
    pub settlement_price: Option<f64>,
    /// Ranked mode only: one scored forecast per option bar, for model evaluation.
    /// Empty for the staged pipeline.
    pub forecasts: Vec<ForecastRecord>,
    /// Per-bar state of each open position — the dynamic exit levels a static
    /// trade record cannot carry.
    pub position_marks: Vec<PositionMark>,
    /// Ranked mode only, and only when `scorer_sample_per_bar > 0`: sampled
    /// candidates priced forward, to test whether the score predicts realized P/L.
    pub scorer_records: Vec<ScorerRecord>,
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
    let mut forecasts: Vec<ForecastRecord> = Vec::new();
    let mut position_marks: Vec<PositionMark> = Vec::new();
    let mut scorer_records: Vec<ScorerRecord> = Vec::new();

    let timeline = series.merged_timeline();
    let mut last_ctx: Option<RankedContext> = None;

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
                if let Intent::Open { .. } = intent
                    && let Some(position) = portfolio.open.last()
                {
                    opened_at_index.insert(position.id, seq);
                }
            }

            // 2. Advance the ranked forecast first, so the exit check can read
            //    it (live POP, momentum). The feature engine still advances once
            //    per bar, in order.
            let next_earnings = series.next_earnings_from(bar.ts.date_naive());
            // The features advance only when a cadence window closes; between,
            // exits read the last forecast and no entry is decided.
            let forecast_bar = match strategy {
                Strategy::Ranked(ranked) => closes_cadence_window(
                    bar.ts,
                    series.bar_minutes,
                    i64::from(ranked.features.cadence_minutes()),
                ),
                Strategy::Staged(_) => true,
            };
            if forecast_bar && let Strategy::Ranked(ranked) = strategy {
                last_ctx = Some(ranked_observe(
                    ranked,
                    &[(series, &bar)],
                    next_earnings,
                    &mut forecasts,
                ));
            }
            let ranked_ctx = last_ctx.as_ref();

            // 3. Mark, updating each position's running peak for the trailing stop.
            portfolio.update_peaks(&bar, &config.costs);
            record_marks(&portfolio, &bar, config, &mut position_marks);
            equity_curve.push(EquityPoint {
                ts: bar.ts,
                equity: portfolio.equity(&bar, &config.costs),
                invested: portfolio.capital_at_risk(),
            });

            // 4. Exits, with the forecast available to model-driven rules.
            let (exiting, synthetic_exiting) = compute_exits(
                &portfolio,
                &bar,
                series.expiry_instant(),
                (next_earnings, series.last_release_before(bar.ts.date_naive())),
                &config.exits,
                ranked_ctx,
            );

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

            // 4. Decide. The staged path evaluates its signal (picking up any
            //    trigger carried from a stock-only bar) and resolves a structure;
            //    the ranked path forecasts a distribution and ranks the ladder.
            //    Both go through the same entry gates.
            let mut entering = None;
            match strategy {
                Strategy::Staged(staged) => {
                    let view = MarketView {
                        symbol: &series.symbol,
                        chain: &bar,
                    };
                    let current_trigger = staged.signal.evaluate(&view);
                    let trigger = current_trigger.or(carried_trigger.take());
                    carried_trigger = None;

                    if let Some(trigger) = trigger {
                        census.triggers += 1;
                        match staged.structure.build(&trigger, &view) {
                            None => census.declined += 1,
                            Some(spec) => match entry_gate(
                                portfolio.open.len(),
                                exiting.len(),
                                config,
                                series.expiry_instant(),
                                bar.ts,
                                next_earnings,
                                EntryTiming::default(),
                            ) {
                                EntryGate::PositionLimit => census.at_position_limit += 1,
                                EntryGate::TooCloseToExpiry => census.too_close_to_expiry += 1,
                                EntryGate::EarningsBlackout => census.earnings_blackout += 1,
                                EntryGate::RsiOutOfBand => census.rsi_out_of_band += 1,
                                EntryGate::NoRecentRsiCross => census.no_recent_rsi_cross += 1,
                                EntryGate::Open => match staged.selector.resolve(&spec, &bar) {
                                    Ok(spread) => {
                                        entering = Some(Intent::Open {
                                            spread,
                                            expiry: bar.expiry,
                                            prediction: None,
                                            bounds: ForecastBounds::default(),
                                    entry_features: None,
                                            allocation: None,
                                            leg_vols: Vec::new(),
                                        })
                                    }
                                    Err(reject) => census.reject(&reject),
                                },
                            },
                        }
                    }
                }
                Strategy::Ranked(_) if !forecast_bar => {}
                Strategy::Ranked(ranked) => {
                    // The forecast was computed in step 2; reuse it.
                    let ctx = ranked_ctx.expect("a forecast bar computed a ctx");

                    match entry_gate(
                        portfolio.open.len(),
                        exiting.len(),
                        config,
                        series.expiry_instant(),
                        bar.ts,
                        next_earnings,
                        EntryTiming {
                            rsi: ctx.features.rsi,
                            bars_since_rsi_cross_up: ctx.features.bars_since_rsi_cross_up,
                        },
                    ) {
                        EntryGate::PositionLimit => census.at_position_limit += 1,
                        EntryGate::TooCloseToExpiry => census.too_close_to_expiry += 1,
                        EntryGate::EarningsBlackout => census.earnings_blackout += 1,
                                EntryGate::RsiOutOfBand => census.rsi_out_of_band += 1,
                                EntryGate::NoRecentRsiCross => census.no_recent_rsi_cross += 1,
                        EntryGate::Open => {
                            if let Some((choice, bounds, expiry)) = ranked_choose(
                                ranked,
                                ctx,
                                &[(series, &bar)],
                                config,
                                &mut census,
                                &mut scorer_records,
                            ) {
                                // Per-symbol path: size at decision time against
                                // this symbol's own portfolio.
                                let lots = match &config.portfolio_model {
                                    None => 1,
                                    Some(model) => model.lots(
                                        AllocationCandidate {
                                            score: choice.score,
                                            one_lot_max_loss: choice.candidate.max_loss,
                                            iv_vs_median: ctx.features.iv_vs_median,
                                        },
                                        PortfolioState::new(
                                            config.initial_cash,
                                            portfolio.capital_at_risk(),
                                            portfolio.open.len(),
                                        ),
                                    ),
                                };
                                if lots == 0 {
                                    // Not enough capital left for even one lot.
                                    census.capital_limited += 1;
                                } else {
                                    let prediction = TradePrediction {
                                        // `expected_pl` scales with size; the
                                        // probability and per-risk score do not.
                                        expected_pl: choice.expected_pl * lots as f64,
                                        score: choice.score,
                                        prob_profit: choice.prob_profit,
                                    };
                                    let leg_vols = choice.candidate.leg_vols.clone();
                                    let spread = if lots == 1 {
                                        choice.candidate.spread
                                    } else {
                                        choice.candidate.spread.scaled(lots)
                                    };
                                    entering = Some(Intent::Open {
                                        spread,
                                        expiry,
                                        prediction: Some(prediction),
                                        bounds,
                                    entry_features: Some(Box::new(ctx.entry_features)),
                                        allocation: None,
                                        leg_vols,
                                    });
                                }
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
                        if let Intent::Open { .. } = intent
                            && let Some(position) = portfolio.open.last()
                        {
                            opened_at_index.insert(position.id, seq);
                        }
                    }
                }
            }
        } else if let Strategy::Staged(staged) = strategy {
            // ---- Stock-only bar: advance the signal, carry any trigger ----
            // Only the staged path uses the finer 5-min stream; the ranked
            // feature engine is driven from the 30-min option bars alone.
            let Some(bar) = series.stock_only_snapshot(ts) else {
                continue;
            };
            let view = MarketView {
                symbol: &series.symbol,
                chain: &bar,
            };
            let trigger = staged.signal.evaluate(&view);
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
        equity_curve.push(EquityPoint {
            ts: series.expiry_instant(),
            equity: portfolio.cash,
            invested: portfolio.capital_at_risk(),
        });
    }

    let final_equity = portfolio.cash;
    for trade in &mut portfolio.closed {
        trade.held_through_earnings = held_through_earnings(trade, &series.earnings);
    }
    let signal_diagnostics = strategy.signal_diagnostics();
    Ok(SymbolRun {
        symbol: series.symbol.clone(),
        strategy: strategy.name().to_string(),
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
        forecasts,
        position_marks,
        scorer_records,
    })
}

/// One symbol's diagnostics from a shared-account run — everything the report
/// shows per symbol except the trades and equity, which live on the single
/// shared portfolio and curve.
pub struct SymbolDiag {
    pub symbol: String,
    pub bars: usize,
    pub census: Census,
    pub signal_diagnostics: BTreeMap<String, usize>,
    pub option_quality: Quality,
    pub underlying_quality: Quality,
    pub forecasts: Vec<ForecastRecord>,
    pub scorer_records: Vec<ScorerRecord>,
    pub position_marks: Vec<PositionMark>,
    pub settlement_price: Option<f64>,
}

/// The result of a shared-account backtest: one portfolio and one equity curve
/// for the whole account, with per-symbol diagnostics beside them.
pub struct SharedRun {
    pub strategy: String,
    /// Every expiry any symbol traded, ascending.
    pub expiries: Vec<NaiveDate>,
    pub initial_cash: f64,
    pub final_equity: f64,
    pub portfolio: Portfolio,
    pub equity_curve: Vec<EquityPoint>,
    pub per_symbol: Vec<SymbolDiag>,
    pub first_ts: Option<DateTime<Utc>>,
    pub last_ts: Option<DateTime<Utc>>,
}

/// The score a queued open sorts by when the shared account funds best-first.
/// Opens with no allocation hint (the staged path) sort last.
fn open_score(intent: &Intent) -> f64 {
    match intent {
        Intent::Open {
            allocation: Some(hint), ..
        } => hint.score,
        _ => f64::MIN,
    }
}

/// Replay every symbol against **one** shared portfolio over a single global
/// timeline merged across all symbols. Capital is a common pool: at each
/// timestamp the entries queued from the previous bar are funded
/// **best-score-first** and sized at fill against the capital still free, so the
/// whole account — not each symbol — is what `initial_cash` bounds. A per-symbol
/// cap keeps it from piling into one name. Next-bar timing throughout.
///
/// `symbols[i]` is every chain of one symbol, ascending by expiry. The symbol's
/// features are read once per bar and each chain gets a forecast capped at its
/// own expiry. Entries go to the front chain outside the entry blackout — or,
/// with `ExpiryChoice::Best`, to the best candidate across every open chain —
/// so different positions sit in different expiries. A position is never rolled:
/// it is marked, exited and closed only in its own chain, and settles at its
/// own expiry while the run carries on.
pub fn run_shared(
    symbols: &[Vec<ChainSeries>],
    strategies: &mut [Strategy],
    config: &RunConfig,
) -> Result<SharedRun> {
    let n = symbols.len();
    let mut portfolio = Portfolio::new(config.initial_cash);
    let mut census: Vec<Census> = (0..n).map(|_| Census::default()).collect();
    let mut pending: Vec<Vec<Intent>> = (0..n).map(|_| Vec::new()).collect();
    let mut carried_trigger: Vec<Option<crate::signal::Trigger>> =
        (0..n).map(|_| None).collect();
    let mut forecasts: Vec<Vec<ForecastRecord>> = (0..n).map(|_| Vec::new()).collect();
    let mut position_marks: Vec<Vec<PositionMark>> = (0..n).map(|_| Vec::new()).collect();
    let mut scorer_records: Vec<Vec<ScorerRecord>> = (0..n).map(|_| Vec::new()).collect();
    // pos id -> the opening symbol's own bar index, for a per-symbol held count.
    let mut opened_at_index: BTreeMap<usize, usize> = BTreeMap::new();
    // pos id -> last mark, so the account curve can value a position between the
    // sparse bars of its own chain.
    let mut marks: BTreeMap<usize, f64> = BTreeMap::new();
    let mut sym_bars: Vec<usize> = vec![0; n];
    let mut equity_curve: Vec<EquityPoint> = Vec::new();
    // Each symbol's forecast from its last closed cadence window.
    let mut last_ctx: Vec<Option<RankedContext>> = (0..n).map(|_| None).collect();
    // Each symbol's latest entry fill, for the ladder's spacing.
    let mut last_entry: Vec<Option<DateTime<Utc>>> = vec![None; n];

    // Merged, ordered global timeline: (ts, symbol, chain, option_index?). The
    // stock-only bars are one tape per symbol, so they come from its first chain.
    let mut global: Vec<(DateTime<Utc>, usize, usize, Option<usize>)> = Vec::new();
    for (si, chains) in symbols.iter().enumerate() {
        for (ci, series) in chains.iter().enumerate() {
            for (ts, oi) in series.merged_timeline() {
                if ci == 0 || oi.is_some() {
                    global.push((ts, si, ci, oi));
                }
            }
        }
    }
    global.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));

    // One position's mark: exit credit net of commission, else held at entry
    // cost — the same rule as `Portfolio::equity`, applied per position.
    let mark_of = |pos: &OpenPosition, bar: &ChainSnapshot| -> f64 {
        match crate::execution::exit_credit(&pos.spread, bar) {
            Ok(credit) => credit - config.costs.commission(&pos.spread),
            Err(_) => pos.entry_debit,
        }
    };

    let mut idx = 0;
    while idx < global.len() {
        let ts = global[idx].0;
        let mut end = idx;
        while end < global.len() && global[end].0 == ts {
            end += 1;
        }

        // 0. A chain that expired before this instant settles its survivors
        //    first, freeing their capital for anything that fills now.
        settle_expired(
            symbols,
            &mut portfolio,
            &mut marks,
            &mut equity_curve,
            &sym_bars,
            &opened_at_index,
            Some(ts),
        );

        // Option bars at this timestamp, by symbol then expiry. A symbol's bar
        // counter advances once per timestamp however many of its chains quote,
        // so held/bars are measured in that symbol's own bars.
        let mut option_bars: Vec<(usize, usize, ChainSnapshot)> = Vec::new();
        for &(_, si, ci, oi) in &global[idx..end] {
            if let Some(index) = oi
                && let Some(bar) = symbols[si][ci].snapshot(index)
            {
                if option_bars.last().is_none_or(|(s, ..)| *s != si) {
                    sym_bars[si] += 1;
                }
                option_bars.push((si, ci, bar));
            }
        }
        // The bar a position or an order trades against: its own chain's.
        let bar_in = |si: usize, expiry: NaiveDate| {
            option_bars
                .iter()
                .find(|(s, _, bar)| *s == si && bar.expiry == expiry)
                .map(|(.., bar)| bar)
        };
        let mut quoting: Vec<usize> = option_bars.iter().map(|&(si, ..)| si).collect();
        quoting.dedup();

        // 1a. Execute due CLOSES per symbol (frees capital), and collect the due
        //     OPENS across symbols for best-first funding.
        let mut open_queue: Vec<(usize, Intent)> = Vec::new();
        for &si in &quoting {
            for intent in std::mem::take(&mut pending[si]) {
                match intent {
                    Intent::Close { id, reason } => {
                        let Some(expiry) = portfolio.position(id).map(|p| p.expiry) else {
                            continue; // already closed
                        };
                        let held = sym_bars[si]
                            .saturating_sub(opened_at_index.get(&id).copied().unwrap_or(sym_bars[si]));
                        match bar_in(si, expiry) {
                            Some(bar) if portfolio.close(id, bar, &config.costs, reason, held).is_ok() => {
                                marks.remove(&id);
                            }
                            _ => census[si].unfillable_intents += 1,
                        }
                    }
                    open => open_queue.push((si, open)),
                }
            }
        }

        // 1b. Fund opens best-score-first against the shared capital.
        open_queue.sort_by(|a, b| {
            open_score(&b.1)
                .partial_cmp(&open_score(&a.1))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        for (si, intent) in open_queue {
            let Intent::Open { expiry, .. } = &intent else {
                unreachable!("closes were executed above");
            };
            let Some(bar) = bar_in(si, *expiry) else {
                census[si].unfillable_intents += 1;
                continue;
            };
            let symbol = &bar.symbol;
            let in_symbol = portfolio
                .open
                .iter()
                .filter(|p| &p.spread.symbol == symbol)
                .count();
            let per_symbol_cap = config.max_positions_per_symbol.unwrap_or(usize::MAX);
            if in_symbol >= per_symbol_cap || portfolio.open.len() >= config.max_open_positions {
                census[si].at_position_limit += 1;
                continue;
            }
            let before = portfolio.open.len();
            execute(
                &intent,
                bar,
                &mut portfolio,
                config,
                &mut census[si],
                &opened_at_index,
                sym_bars[si],
            )?;
            if portfolio.open.len() > before {
                let position = portfolio.open.last().expect("just opened");
                opened_at_index.insert(position.id, sym_bars[si]);
                marks.insert(position.id, mark_of(position, bar));
                last_entry[si] = Some(position.opened_at);
            }
        }

        // 2. Mark every chain's positions against its bar (also updating each
        //    position's running peak for the trailing stop), then record one
        //    account equity point for this timestamp.
        for (si, _, bar) in &option_bars {
            portfolio.update_peaks(bar, &config.costs);
            record_marks(&portfolio, bar, config, &mut position_marks[*si]);
        }
        for (.., bar) in &option_bars {
            for pos in portfolio
                .open
                .iter()
                .filter(|p| p.spread.symbol == bar.symbol && p.expiry == bar.expiry)
            {
                marks.insert(pos.id, mark_of(pos, bar));
            }
        }
        if !option_bars.is_empty() {
            let equity = portfolio.cash + marks.values().sum::<f64>();
            equity_curve.push(EquityPoint {
                ts,
                equity,
                invested: portfolio.capital_at_risk(),
            });
        }

        // 3. Decide per symbol: exits (queue real closes, execute synthetic) then
        //    the entry, queued for that symbol's next option bar.
        for &si in &quoting {
            let chains: Vec<(&ChainSeries, &ChainSnapshot)> = option_bars
                .iter()
                .filter(|(s, ..)| *s == si)
                .map(|(_, ci, bar)| (&symbols[si][*ci], bar))
                .collect();
            let next_earnings = symbols[si][0].next_earnings_from(ts.date_naive());
            // Advance the forecast first so exits can read it (live POP,
            // momentum) — but only when a cadence window closes. Between, exits
            // read the last forecast and no entry is decided.
            let forecast_bar = match &strategies[si] {
                Strategy::Ranked(ranked) => closes_cadence_window(
                    ts,
                    chains[0].0.bar_minutes,
                    i64::from(ranked.features.cadence_minutes()),
                ),
                Strategy::Staged(_) => true,
            };
            if forecast_bar && let Strategy::Ranked(ranked) = &mut strategies[si] {
                last_ctx[si] = Some(ranked_observe(
                    ranked,
                    &chains,
                    next_earnings,
                    &mut forecasts[si],
                ));
            }
            let ranked_ctx = last_ctx[si].as_ref();
            let mut exiting = Vec::new();
            for &(series, bar) in &chains {
                let (queued, synthetic) = compute_exits(
                    &portfolio,
                    bar,
                    series.expiry_instant(),
                    (next_earnings, series.last_release_before(ts.date_naive())),
                    &config.exits,
                    ranked_ctx,
                );
                for (id, reason) in &synthetic {
                    if let Some(spot) = bar.underlying.price() {
                        let held = sym_bars[si]
                            .saturating_sub(opened_at_index.get(id).copied().unwrap_or(sym_bars[si]));
                        portfolio.close_at_intrinsic(*id, spot, bar.ts, &config.costs, *reason, held);
                        census[si].synthetic_exits += 1;
                        marks.remove(id);
                    }
                }
                exiting.extend(queued);
            }

            let mut entering: Option<Intent> = None;
            match &mut strategies[si] {
                Strategy::Ranked(_) if !forecast_bar => {}
                Strategy::Ranked(ranked) => {
                    let ctx = ranked_ctx.expect("a forecast bar computed a ctx");
                    // The gate asks whether *any* chain is still open to entry;
                    // which ones are is decided chain by chain below.
                    let farthest = chains.last().expect("a quoting symbol has a chain").0;
                    match entry_gate(
                        portfolio.open.len(),
                        exiting.len(),
                        config,
                        farthest.expiry_instant(),
                        ts,
                        next_earnings,
                        EntryTiming {
                            rsi: ctx.features.rsi,
                            bars_since_rsi_cross_up: ctx.features.bars_since_rsi_cross_up,
                        },
                    ) {
                        EntryGate::PositionLimit => census[si].at_position_limit += 1,
                        EntryGate::TooCloseToExpiry => census[si].too_close_to_expiry += 1,
                        EntryGate::EarningsBlackout => census[si].earnings_blackout += 1,
                        EntryGate::RsiOutOfBand => census[si].rsi_out_of_band += 1,
                        EntryGate::NoRecentRsiCross => census[si].no_recent_rsi_cross += 1,
                        EntryGate::Open if !inside_entry_window(config, ts) => {
                            census[si].outside_entry_window += 1
                        }
                        EntryGate::Open
                            if config.min_days_between_entries.is_some_and(|days| {
                                last_entry[si].is_some_and(|at| {
                                    ((ts - at).num_seconds() as f64) < days * 86_400.0
                                })
                            }) =>
                        {
                            census[si].entry_spacing += 1
                        }
                        EntryGate::Open => {
                            let mut open: Vec<(&ChainSeries, &ChainSnapshot)> = chains
                                .iter()
                                .filter(|(series, _)| {
                                    !too_close_to_expiry(config, series.expiry_instant(), ts)
                                })
                                .copied()
                                .collect();
                            // Ascending by expiry, so the first is the front month.
                            if config.expiry_choice == ExpiryChoice::Front {
                                open.truncate(1);
                            }
                            if let Some((choice, bounds, expiry)) = ranked_choose(
                                ranked,
                                ctx,
                                &open,
                                config,
                                &mut census[si],
                                &mut scorer_records[si],
                            ) {
                                // Defer allocation to fill only when a portfolio
                                // model is configured; otherwise use one lot.
                                let allocation = config.portfolio_model.map(|_| AllocationHint {
                                    score: choice.score,
                                    one_lot_max_loss: choice.candidate.max_loss,
                                    iv_vs_median: ctx.features.iv_vs_median,
                                    expected_pl: choice.expected_pl,
                                });
                                let prediction = TradePrediction {
                                    expected_pl: choice.expected_pl,
                                    score: choice.score,
                                    prob_profit: choice.prob_profit,
                                };
                                entering = Some(Intent::Open {
                                    leg_vols: choice.candidate.leg_vols.clone(),
                                    spread: choice.candidate.spread,
                                    expiry,
                                    prediction: Some(prediction),
                                    bounds,
                                    entry_features: Some(Box::new(ctx.entry_features)),
                                    allocation,
                                });
                            }
                        }
                    }
                }
                Strategy::Staged(staged) => {
                    // Staged runs are single-expiry (`validate` refuses more).
                    let (series, bar) = chains[0];
                    let view = MarketView {
                        symbol: &series.symbol,
                        chain: bar,
                    };
                    let current = staged.signal.evaluate(&view);
                    let trigger = current.or(carried_trigger[si].take());
                    carried_trigger[si] = None;
                    if let Some(trigger) = trigger {
                        census[si].triggers += 1;
                        match staged.structure.build(&trigger, &view) {
                            None => census[si].declined += 1,
                            Some(spec) => match entry_gate(
                                portfolio.open.len(),
                                exiting.len(),
                                config,
                                series.expiry_instant(),
                                bar.ts,
                                next_earnings,
                                EntryTiming::default(),
                            ) {
                                EntryGate::PositionLimit => census[si].at_position_limit += 1,
                                EntryGate::TooCloseToExpiry => census[si].too_close_to_expiry += 1,
                                EntryGate::EarningsBlackout => census[si].earnings_blackout += 1,
                                EntryGate::RsiOutOfBand => census[si].rsi_out_of_band += 1,
                                EntryGate::NoRecentRsiCross => census[si].no_recent_rsi_cross += 1,
                                EntryGate::Open => match staged.selector.resolve(&spec, bar) {
                                    Ok(spread) => {
                                        entering = Some(Intent::Open {
                                            spread,
                                            expiry: bar.expiry,
                                            prediction: None,
                                            bounds: ForecastBounds::default(),
                                            entry_features: None,
                                            allocation: None,
                                            leg_vols: Vec::new(),
                                        })
                                    }
                                    Err(reject) => census[si].reject(&reject),
                                },
                            },
                        }
                    }
                }
            }
            exiting.extend(entering);
            pending[si] = exiting;
        }

        // 4. Stock-only bars. A ranked run with `stock_bar_exits` watches the
        //    price-level exits on them, closing on the next option bar; the
        //    staged path advances its signal and carries any trigger.
        for &(_, si, ci, oi) in &global[idx..end] {
            if oi.is_none()
                && config.exits.stock_bar_exits
                && matches!(strategies[si], Strategy::Ranked(_))
                && let Some(spot) = symbols[si][ci].underlying_asof(ts).and_then(|b| b.price())
            {
                watch_stock_bar(&portfolio, &symbols[si][ci].symbol, spot, &config.exits, &mut pending[si]);
            }
            if oi.is_none()
                && let Strategy::Staged(staged) = &mut strategies[si]
                && let Some(bar) = symbols[si][ci].stock_only_snapshot(ts)
            {
                let view = MarketView {
                    symbol: &symbols[si][ci].symbol,
                    chain: &bar,
                };
                let trigger = staged.signal.evaluate(&view);
                if trigger.is_some() {
                    carried_trigger[si] = trigger;
                }
            }
        }

        idx = end;
    }

    // Whatever survived its tape settles at intrinsic against its own chain's
    // expiry print. A chain still live when the data ends stays open, marked.
    settle_expired(
        symbols,
        &mut portfolio,
        &mut marks,
        &mut equity_curve,
        &sym_bars,
        &opened_at_index,
        None,
    );
    let final_equity = portfolio.cash
        + portfolio
            .open
            .iter()
            .filter_map(|p| marks.get(&p.id))
            .sum::<f64>();
    for trade in &mut portfolio.closed {
        if let Some(chains) = symbols.iter().find(|c| c[0].symbol == trade.symbol) {
            trade.held_through_earnings = held_through_earnings(trade, &chains[0].earnings);
        }
    }

    let strategy_name = strategies
        .first()
        .map(|s| s.name().to_string())
        .unwrap_or_default();
    let per_symbol: Vec<SymbolDiag> = (0..n)
        .map(|si| {
            let chains = &symbols[si];
            let mut option_quality = Quality::default();
            for chain in chains {
                option_quality.add(&chain.option_quality);
            }
            SymbolDiag {
                symbol: chains[0].symbol.clone(),
                bars: global.iter().filter(|e| e.1 == si).count(),
                census: std::mem::take(&mut census[si]),
                signal_diagnostics: strategies[si].signal_diagnostics(),
                option_quality,
                underlying_quality: chains[0].underlying_quality,
                forecasts: std::mem::take(&mut forecasts[si]),
                position_marks: std::mem::take(&mut position_marks[si]),
                scorer_records: std::mem::take(&mut scorer_records[si]),
                settlement_price: chains[0]
                    .underlying_asof(chains[0].expiry_instant())
                    .and_then(|b| b.price()),
            }
        })
        .collect();
    let mut expiries: Vec<NaiveDate> = symbols.iter().flatten().map(|s| s.expiry).collect();
    expiries.sort();
    expiries.dedup();

    Ok(SharedRun {
        strategy: strategy_name,
        expiries,
        initial_cash: config.initial_cash,
        final_equity,
        portfolio,
        equity_curve,
        per_symbol,
        first_ts: global.first().map(|&(ts, ..)| ts),
        last_ts: global.last().map(|&(ts, ..)| ts),
    })
}

/// Whether a trade was open across an earnings release: opened on or before the
/// release's last clean session, closed after it.
fn held_through_earnings(trade: &crate::portfolio::ClosedTrade, events: &[EarningsEvent]) -> bool {
    held_across(trade.opened_at.date_naive(), trade.closed_at.date_naive(), events)
}

fn held_across(opened: NaiveDate, closed: NaiveDate, events: &[EarningsEvent]) -> bool {
    events.iter().any(|e| opened <= e.close_on && closed > e.close_on)
}

/// Queue a close for each open position of `symbol` whose price-level exit this
/// stock bar has crossed, unless one is already queued. It fills on the
/// position's next option bar — the first quote after the crossing, up to one
/// option bar sooner than waiting to see the crossing on an option bar itself.
fn watch_stock_bar(
    portfolio: &Portfolio,
    symbol: &str,
    spot: f64,
    exits: &ExitRules,
    pending: &mut Vec<Intent>,
) {
    for position in portfolio.open.iter().filter(|p| p.spread.symbol == symbol) {
        let queued = pending
            .iter()
            .any(|intent| matches!(intent, Intent::Close { id, .. } if *id == position.id));
        if !queued && let Some(reason) = exits.price_triggered(position, spot) {
            pending.push(Intent::Close {
                id: position.id,
                reason,
            });
        }
    }
}

/// Settle every open position whose chain expired before `before` (all of them
/// when `None`) at that chain's expiry-day print, and record the account at the
/// settlement instant. A chain whose tape never reached expiry day — one still
/// live when the capture ended — has no print, and its positions stay open.
fn settle_expired(
    symbols: &[Vec<ChainSeries>],
    portfolio: &mut Portfolio,
    marks: &mut BTreeMap<usize, f64>,
    equity_curve: &mut Vec<EquityPoint>,
    sym_bars: &[usize],
    opened_at_index: &BTreeMap<usize, usize>,
    before: Option<DateTime<Utc>>,
) {
    let due: Vec<(usize, usize, f64, DateTime<Utc>)> = portfolio
        .open
        .iter()
        .filter_map(|p| {
            let (si, chain) = symbols.iter().enumerate().find_map(|(si, chains)| {
                chains
                    .iter()
                    .find(|c| c.symbol == p.spread.symbol && c.expiry == p.expiry)
                    .map(|c| (si, c))
            })?;
            let at = chain.expiry_instant();
            if before.is_some_and(|b| at >= b) {
                return None;
            }
            Some((p.id, si, chain.settlement_price()?, at))
        })
        .collect();
    let mut last = None;
    for (id, si, price, at) in due {
        let held = sym_bars[si].saturating_sub(opened_at_index.get(&id).copied().unwrap_or(0));
        portfolio.settle_at_expiry(id, price, at, held);
        marks.remove(&id);
        last = last.max(Some(at));
    }
    if let Some(at) = last {
        equity_curve.push(EquityPoint {
            ts: at,
            equity: portfolio.cash + marks.values().sum::<f64>(),
            invested: portfolio.capital_at_risk(),
        });
    }
}

/// Exits for the open positions in `bar`'s chain — same symbol, same expiry —
/// evaluated against `bar`. Returns the closes to queue (priced on a real quote)
/// and the intrinsic closes to execute immediately (the position's legs stopped
/// quoting). A position is only ever read against its own chain: another
/// expiry's bar does not quote its legs, and would look like a tape that stopped.
fn compute_exits(
    portfolio: &Portfolio,
    bar: &ChainSnapshot,
    expiry_instant: DateTime<Utc>,
    (next_earnings, last_release): (Option<&EarningsEvent>, Option<&EarningsEvent>),
    exits: &ExitRules,
    model: Option<&RankedContext>,
) -> (Vec<Intent>, Vec<(usize, ExitReason)>) {
    let mut exiting = Vec::new();
    let mut synthetic = Vec::new();
    let in_chain = |p: &&OpenPosition| p.spread.symbol == bar.symbol && p.expiry == bar.expiry;
    for position in portfolio.open.iter().filter(in_chain) {
        match crate::execution::exit_credit(&position.spread, bar) {
            Ok(credit) => {
                let gross = credit - position.entry_debit;
                let ctx = exits::ExitContext {
                    gross_pnl: gross,
                    underlying_price: bar.underlying.price(),
                    bar_ts: bar.ts,
                    expiry_instant,
                    next_earnings,
                    last_release,
                    model: model.and_then(|ctx| ctx.exit_model(bar.expiry)),
                };
                if let Some(reason) = exits.triggered(position, &ctx) {
                    exiting.push(Intent::Close {
                        id: position.id,
                        reason,
                    });
                }
            }
            Err(_) => {
                let reason = bar
                    .underlying
                    .price()
                    .and_then(|spot| exits.underlying_price_triggered(position, spot))
                    .or_else(|| {
                        exits.time_triggered(
                            position.opened_at,
                            bar.ts,
                            expiry_instant,
                            next_earnings,
                            last_release,
                        )
                    });
                if let Some(reason) = reason {
                    synthetic.push((position.id, reason));
                }
            }
        }
    }
    (exiting, synthetic)
}

/// Everything a ranked option bar computes before the entry gate: the symbol's
/// features, read once, and the forecast as each chain quoting this bar sees it.
/// This advances the feature engine — so it MUST run on every option bar, gate
/// or no gate, in both the per-symbol and shared-account loops.
struct RankedContext {
    features: Features,
    /// One per chain quoting the bar it was computed on, keyed by expiry, so a
    /// later option bar inside the same cadence window can still find its own.
    chains: Vec<(NaiveDate, ChainForecast)>,
    full_horizon_years: f64,
    /// What the model saw this bar, recorded onto any position opened from it.
    entry_features: crate::model::EntryFeatures,
}

/// The forecast as one chain sees it. The features are the symbol's, but the
/// horizon is capped at *this* chain's expiry and a sampled spot maps to *this*
/// chain's forward, so a candidate or a position is always marked against its
/// own expiry.
struct ChainForecast {
    dist: PriceDistribution,
    /// Years from the horizon to the chain's expiry (zero scores at expiry),
    /// scaled by the square of the model's `horizon_vol_scale` — the crush.
    remaining_years: f64,
    /// Sampled-spot -> forward scale, preserving the chain's parity basis.
    forward_scale: f64,
}

impl RankedContext {
    fn forecast(&self, expiry: NaiveDate) -> Option<&ChainForecast> {
        self.chains.iter().find(|(e, _)| *e == expiry).map(|(_, f)| f)
    }

    /// The forecast for the chain expiring `expiry`, packaged for the exit check.
    fn exit_model(&self, expiry: NaiveDate) -> Option<exits::ExitModel<'_>> {
        let chain = self.forecast(expiry)?;
        Some(exits::ExitModel {
            dist: &chain.dist,
            remaining_years: chain.remaining_years,
            forward_scale: chain.forward_scale,
            trend: self.features.trend,
        })
    }
}

/// Whether the option bar starting at `ts` closes a feature-cadence window —
/// its end falls on a `cadence`-minute boundary. Every bar does when the tape is
/// no finer than the cadence, which is what keeps a 30-minute model on 30-minute
/// data exactly as it was.
fn closes_cadence_window(ts: DateTime<Utc>, bar_minutes: i64, cadence: i64) -> bool {
    let end_minute = (ts + Duration::minutes(bar_minutes)).timestamp() / 60;
    bar_minutes >= cadence || end_minute % cadence == 0
}

/// Which chain feeds the features: the nearest expiry that still covers the
/// horizon, so its ATM IV is the vol of the period being forecast — failing
/// that, the farthest. `chains` ascends by expiry; with one chain, that chain.
fn feature_chain(chains: &[(&ChainSeries, &ChainSnapshot)], horizon_years: f64) -> usize {
    chains
        .iter()
        .position(|(_, bar)| bar.year_fraction >= horizon_years)
        .unwrap_or(chains.len() - 1)
}

/// `chains` is every chain of one symbol quoting this bar, ascending by expiry.
fn ranked_observe(
    ranked: &mut RankedStrategy,
    chains: &[(&ChainSeries, &ChainSnapshot)],
    next_earnings: Option<&EarningsEvent>,
    forecasts: &mut Vec<ForecastRecord>,
) -> RankedContext {
    let full_horizon_years = ranked.model.horizon_days() / 365.25;
    let feature = feature_chain(chains, full_horizon_years);
    let (series, bar) = chains[feature];
    // On a tape finer than the cadence the indicators read the whole window's
    // stock bar, not the last option bar's slice of it — an ATR over 15-minute
    // ranges is a different indicator from the one validated on 30.
    let cadence = i64::from(ranked.features.cadence_minutes());
    let windowed;
    let bar = match (series.bar_minutes < cadence)
        .then(|| series.window_bar(bar.ts + Duration::minutes(series.bar_minutes), cadence))
        .flatten()
    {
        Some(underlying) => {
            windowed = ChainSnapshot {
                underlying,
                ..bar.clone()
            };
            &windowed
        }
        None => bar,
    };
    let bar_date = bar.ts.date_naive();
    let earnings = EarningsDistance {
        days_to: next_earnings.map(|e| (e.date - bar_date).num_days()),
        days_since: series
            .previous_earnings_from(bar_date)
            .map(|e| (bar_date - e.date).num_days()),
    };
    let features = ranked.features.observe(bar, earnings);

    let forecasts_by_chain: Vec<(NaiveDate, ChainForecast)> = chains
        .iter()
        .map(|(_, chain)| {
            let horizon_years = full_horizon_years.min(chain.year_fraction).max(0.0);
            // Black-76 here reads vol and time only through σ√τ, so revaluing
            // every leg at k × its entry vol is exactly revaluing it at k²τ.
            let crush = ranked.model.horizon_vol_scale(&features, horizon_years);
            let forecast = ChainForecast {
                dist: ranked.model.forecast(&features, horizon_years),
                remaining_years: (chain.year_fraction - horizon_years).max(0.0) * crush * crush,
                forward_scale: if features.spot > 0.0 {
                    chain.forward / features.spot
                } else {
                    1.0
                },
            };
            (chain.expiry, forecast)
        })
        .collect();

    // Score the forecast against what the underlying actually did a horizon
    // later. Eval-only — never on the decision path. Uses the *full* (uncapped)
    // horizon so the realized read lands on a fixed same-weekday bar.
    if let Some(record) = score_forecast(
        series,
        bar,
        &features,
        &forecasts_by_chain[feature].1.dist,
        full_horizon_years,
        ranked.model.as_ref(),
    ) {
        forecasts.push(record);
    }

    RankedContext {
        features,
        chains: forecasts_by_chain,
        full_horizon_years,
        entry_features: (&features).into(),
    }
}

/// Candidate enumeration, scorer sampling, and ranking for one symbol-bar, once
/// the entry gate has said Open. Each chain in `open` is ranked against its own
/// forecast, and the best
/// candidate across them wins by the ranker's own key, so a position opens in
/// whichever expiry offers the best trade. Returns the best **one-lot** choice,
/// its forecast bounds and its expiry; allocation is the caller's, so it can
/// differ by portfolio model.
fn ranked_choose(
    ranked: &RankedStrategy,
    ctx: &RankedContext,
    open: &[(&ChainSeries, &ChainSnapshot)],
    config: &RunConfig,
    census: &mut Census,
    scorer_records: &mut Vec<ScorerRecord>,
) -> Option<(crate::model::RankedChoice, ForecastBounds, NaiveDate)> {
    let cfg = &ranked.ranking;
    let mut funnel = RankingFunnel::default();
    let mut best: Option<(crate::model::RankedChoice, NaiveDate)> = None;
    for &(series, bar) in open {
        let Some(forecast) = ctx.forecast(bar.expiry) else {
            continue;
        };
        let mut chain_funnel = RankingFunnel::default();
        let candidates =
            enumerate_verticals(bar, &config.costs, &ranked.candidates, &mut chain_funnel);
        // Scorer evaluation: sample candidates and price them forward to grade
        // the score. Reads the candidates before `rank` consumes them; eval-only.
        scorer_sample(
            series,
            bar,
            &candidates,
            forecast,
            ctx.full_horizon_years,
            cfg,
            scorer_records,
        );
        let choice = rank(
            candidates,
            &forecast.dist,
            forecast.remaining_years,
            forecast.forward_scale,
            cfg,
            &mut chain_funnel,
        );
        funnel.merge(&chain_funnel);
        if let Some(choice) = choice
            && best
                .as_ref()
                .is_none_or(|(held, ..)| choice_key(&choice, cfg) > choice_key(held, cfg))
        {
            best = Some((choice, bar.expiry));
        }
    }
    // A bar takes at most one candidate, however many chains offered one.
    funnel.chosen = usize::from(best.is_some());
    census.ranking.merge(&funnel);

    let (choice, expiry) = best?;
    // Both bounds of the entry distribution, read in the trade's direction: the
    // favourable quantile it takes profit at, and the adverse one past which the
    // forecast no longer covers it. Prices, so lot count never touches either.
    let dist = &ctx.forecast(expiry).expect("the choice came from a forecast chain").dist;
    // A direction-neutral structure has no side of the distribution to aim at.
    if choice.candidate.spread.is_direction_neutral() {
        return Some((choice, ForecastBounds::default(), expiry));
    }
    let rises = choice.candidate.spread.profits_from_a_rise();
    let quantile_toward = |q: f64| dist.quantile(if rises { q } else { 1.0 - q });
    let bounds = ForecastBounds {
        target: config.exits.forecast_target_quantile.map(&quantile_toward),
        // The adverse side is the mirror image: 0.85 means the 85th percentile
        // of the move *against* the position.
        stop: config
            .exits
            .forecast_stop_quantile
            .map(|q| quantile_toward(1.0 - q)),
    };
    Some((choice, bounds, expiry))
}

/// Sample up to `sample_per_bar` candidates (a deterministic stride across the
/// enumerated list, so it spans widths and strikes) and price each forward to the
/// option bar exactly a horizon later, to test whether the score predicts the
/// realized outcome. Eval-only — never touches a decision. Skips the bar when
/// there is no option bar a horizon out (a holiday, or past the last one near
/// expiry).
#[allow(clippy::too_many_arguments)]
fn scorer_sample(
    series: &ChainSeries,
    bar: &ChainSnapshot,
    candidates: &[Candidate],
    forecast: &ChainForecast,
    full_horizon_years: f64,
    cfg: &RankingConfig,
    out: &mut Vec<ScorerRecord>,
) {
    let ChainForecast {
        dist,
        remaining_years,
        forward_scale,
    } = forecast;
    let (remaining_years, forward_scale) = (*remaining_years, *forward_scale);
    let (forecast_target_quantile, sample_per_bar) =
        (cfg.forecast_target_quantile, cfg.scorer_sample_per_bar);
    if sample_per_bar == 0 || candidates.is_empty() {
        return;
    }
    let horizon_secs = (full_horizon_years * SECONDS_PER_YEAR) as i64;
    let horizon_ts = bar.ts + Duration::seconds(horizon_secs);
    let Some(horizon_chain) = series.snapshot_at(horizon_ts) else {
        return;
    };
    // Option bars strictly after entry, up to the horizon — the window a
    // forecast-target exit could fire in.
    let future_bars: Vec<DateTime<Utc>> = series
        .timestamps()
        .filter(|&ts| ts > bar.ts && ts <= horizon_ts)
        .collect();

    let close_value = |chain: &ChainSnapshot, candidate: &Candidate| {
        crate::execution::exit_credit(&candidate.spread, chain)
            .unwrap_or_else(|_| candidate.spread.payoff_at(chain.reference_price()))
    };

    let stride = (candidates.len() / sample_per_bar).max(1);
    for candidate in candidates.iter().step_by(stride).take(sample_per_bar) {
        // Grade the number this ranker orders by, which is the metric's key —
        // not always the expected-P/L ratio.
        let (expected_pl, score) =
            ranking_key(candidate, dist, remaining_years, forward_scale, cfg);
        // Recorded regardless of the metric: calibrating a probability needs the
        // probability, not whichever number this ranker happened to order by.
        let prob_profit = crate::model::prob_profit_of(
            &candidate.spread,
            candidate.cost_basis + candidate.exit_spread_cost,
            &candidate.leg_vols,
            dist,
            remaining_years,
            forward_scale,
        );
        // Realized: close the way the strategy would — at the forecast target on
        // the first bar the underlying reaches it, else on the horizon bar. The
        // entry cost is already in `cost_basis`; a leg that stopped quoting falls
        // back to intrinsic.
        let target_close = forecast_target_quantile.and_then(|q| {
            let bullish = candidate.spread.profits_from_a_rise();
            let target = dist.quantile(if bullish { q } else { 1.0 - q });
            future_bars.iter().find_map(|&ts| {
                let spot = series.underlying_at(ts)?.price()?;
                let reached = if bullish { spot >= target } else { spot <= target };
                let chain = reached.then(|| series.snapshot_at(ts))??;
                Some(close_value(&chain, candidate))
            })
        });
        let realized_value = target_close.unwrap_or_else(|| close_value(&horizon_chain, candidate));
        let realized_pl = realized_value - candidate.cost_basis;
        out.push(ScorerRecord {
            ts: bar.ts,
            symbol: series.symbol.clone(),
            expiry: series.expiry,
            score,
            expected_pl,
            realized_pl,
            max_loss: candidate.max_loss,
            entry_debit: candidate.entry_debit,
            legs: candidate.spread.legs.len() as u8,
            prob_profit,
        });
    }
}

/// Score one bar's forecast against the underlying's realized price a horizon
/// later, plus a random-walk null. Returns `None` when the horizon is degenerate
/// (capped to ~0 at expiry) or the realized print is unavailable.
///
/// The realized read looks *forward* in the already-loaded series. This is safe
/// only because the result feeds evaluation and never a decision — see
/// [`crate::model::eval`]. Do not reuse it where a trade could see it.
fn score_forecast(
    series: &ChainSeries,
    bar: &ChainSnapshot,
    features: &Features,
    dist: &PriceDistribution,
    horizon_years: f64,
    model: &dyn PriceModel,
) -> Option<ForecastRecord> {
    // Read the realized underlying at *exactly* the horizon offset. A 7-day
    // horizon lands on the same weekday and time a week later, which is a real
    // trading bar — so an exact lookup gives every intraday forecast a distinct
    // target and never resolves into a closed weekend. When the target is a
    // holiday, a missing slot, or past the loaded window (the last week before
    // expiry), the exact lookup misses and the bar is not scored.
    let horizon_secs = (horizon_years * SECONDS_PER_YEAR) as i64;
    let realized = series
        .underlying_at(bar.ts + Duration::seconds(horizon_secs))?
        .price()?;

    // The random-walk null: no drift, vol = ATM IV.
    let null = model.null_forecast(features, horizon_years);

    Some(ForecastRecord::score(
        bar.ts,
        &series.symbol,
        bar.expiry,
        features.spot,
        features.forward,
        features.atm_iv,
        horizon_years,
        features.into(),
        dist,
        &null,
        realized,
    ))
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
        Intent::Open {
            spread,
            prediction,
            bounds,
            entry_features,
            allocation,
            leg_vols,
            expiry: _,
        } => {
            // A deferred allocation hint means `spread` is one lot and the
            // shared account funds it now against capital committed across all
            // symbols. Zero lots means the policy declines the fill.
            let (sized_spread, prediction) = match allocation {
                None => (spread.clone(), *prediction),
                Some(hint) => {
                    let lots = match &config.portfolio_model {
                        None => 1,
                        Some(model) => {
                            let symbol_open_positions = portfolio
                                .open
                                .iter()
                                .filter(|p| p.spread.symbol == spread.symbol)
                                .count();
                            model.lots(
                                AllocationCandidate {
                                    score: hint.score,
                                    one_lot_max_loss: hint.one_lot_max_loss,
                                    iv_vs_median: hint.iv_vs_median,
                                },
                                PortfolioState::new(
                                    config.initial_cash,
                                    portfolio.capital_at_risk(),
                                    symbol_open_positions,
                                ),
                            )
                        }
                    };
                    if lots == 0 {
                        census.capital_limited += 1;
                        return Ok(());
                    }
                    let scaled = prediction.map(|mut p| {
                        p.expected_pl = hint.expected_pl * lots as f64;
                        p
                    });
                    let spread = if lots == 1 {
                        spread.clone()
                    } else {
                        spread.scaled(lots)
                    };
                    (spread, scaled)
                }
            };
            // The bar that arrives may not quote what the previous one did. A
            // missed fill is a real outcome, counted rather than forced.
            match portfolio.open(sized_spread, bar, &config.costs) {
                Ok(_) => {
                    if let Some(position) = portfolio.open.last_mut() {
                        position.prediction = prediction;
                        position.forecast_target = bounds.target;
                        position.forecast_stop = bounds.stop;
                        position.entry_features = entry_features.as_deref().copied();
                        position.leg_vols = leg_vols.clone();
                    }
                }
                Err(_) => census.unfillable_intents += 1,
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
        snapshot_with_chains(name, path, settlement, &[expiry()], 30)
    }

    /// `snapshot_with`, but with a full ladder for every expiry in `expiries`,
    /// each priced to its own time to expiry and settling at `settlement`, on
    /// `bar_minutes`-wide bars (stock and options alike).
    pub fn snapshot_with_chains(
        name: &str,
        path: &[f64],
        settlement: f64,
        expiries: &[NaiveDate],
        bar_minutes: i64,
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
             CREATE TABLE IF NOT EXISTS events (id INTEGER PRIMARY KEY, symbol TEXT,
                 event_date TEXT NOT NULL, event_time TEXT, session_timing TEXT,
                 kind TEXT NOT NULL, source TEXT NOT NULL, source_id TEXT, title TEXT, detail_json TEXT);
             INSERT INTO contracts (id, symbol, kind, exchange, currency, expiry, strike, right)
             VALUES (1, 'TEST', 'stock', 'SMART', 'USD', NULL, NULL, NULL);",
        )
        .unwrap();

        let strikes: Vec<f64> = (35..=65).map(|i| i as f64 * 2.0).collect();
        let mut next_id = 2i64;
        let mut ids = Vec::new();
        for &expiry in expiries {
            let expiry_text = expiry.format("%Y-%m-%d").to_string();
            let expiry_instant = pricing::expiry_instant(expiry).unwrap();
            for &strike in &strikes {
                for right in ["C", "P"] {
                    conn.execute(
                        "INSERT INTO contracts (id, symbol, kind, exchange, currency, expiry, strike, right)
                         VALUES (?1, 'TEST', 'option', 'SMART', 'USD', ?2, ?3, ?4)",
                        rusqlite::params![next_id, expiry_text, strike, right],
                    )
                    .unwrap();
                    ids.push((next_id, strike, right == "C", expiry_instant));
                    next_id += 1;
                }
            }
        }

        let start = Utc.with_ymd_and_hms(2026, 7, 1, 13, 30, 0).unwrap();
        let bar_size = format!("{bar_minutes}min");
        conn.execute_batch("BEGIN").unwrap();
        let mut insert = conn
            .prepare(
                "INSERT INTO bars (contract_id, ts, bar_size, data_type, open, high, low, close, volume)
                 VALUES (?1, ?2, ?5, ?3, ?4, ?4, ?4, ?4, 0)",
            )
            .unwrap();

        for (bar, &spot) in path.iter().enumerate() {
            let ts = (start + Duration::minutes(bar_minutes * bar as i64))
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, false);
            for (kind, price) in [("trades", spot), ("bid", spot - 0.01), ("ask", spot + 0.01)] {
                insert
                    .execute(rusqlite::params![1i64, &ts, kind, price, bar_size])
                    .unwrap();
            }
            let now = start + Duration::minutes(bar_minutes * bar as i64);
            for &(id, strike, is_call, expiry_instant) in &ids {
                let t = pricing::year_fraction(now, expiry_instant);
                // Floor the mid so a deep wing still quotes a positive book,
                // as a real one does.
                let mid = pricing::black76(spot, strike, t, VOL, is_call).max(0.06);
                insert
                    .execute(rusqlite::params![id, &ts, "bid", mid - 0.05, bar_size])
                    .unwrap();
                insert
                    .execute(rusqlite::params![id, &ts, "ask", mid + 0.05, bar_size])
                    .unwrap();
            }
        }

        // The stock print on each expiry day that settlement reads.
        for expiry in expiries {
            let settle_ts = expiry
                .and_hms_opt(19, 30, 0)
                .unwrap()
                .and_utc()
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, false);
            insert
                .execute(rusqlite::params![1i64, &settle_ts, "trades", settlement, bar_size])
                .unwrap();
        }

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
             CREATE TABLE IF NOT EXISTS events (id INTEGER PRIMARY KEY, symbol TEXT,
                 event_date TEXT NOT NULL, event_time TEXT, session_timing TEXT,
                 kind TEXT NOT NULL, source TEXT NOT NULL, source_id TEXT, title TEXT, detail_json TEXT);
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
        Strategy::Staged(StagedStrategy {
            name: "rsi-bull-call".into(),
            signal: Box::new(RsiCross::new(14, 30.0).unwrap()),
            structure: Box::new(Vertical::bull_call(0.50, 0.30, 1).unwrap()),
            selector: Box::new(DeltaSelector {
                delta_tolerance: 0.07,
                max_debit: None,
            }),
        })
    }

    /// The staged stages of a test strategy, for tests that swap one out.
    pub fn staged_mut(strategy: &mut Strategy) -> &mut StagedStrategy {
        match strategy {
            Strategy::Staged(staged) => staged,
            Strategy::Ranked(_) => panic!("expected a staged strategy"),
        }
    }
}


#[cfg(test)]
mod rsi_gate_tests {
    use super::*;
    use chrono::TimeZone;

    fn cfg(min: Option<f64>, max: Option<f64>) -> RunConfig {
        RunConfig {
            min_entry_rsi: min,
            max_entry_rsi: max,
            max_open_positions: 10,
            min_entry_days_before_expiry: None,
            ..RunConfig::default()
        }
    }

    /// The gate refuses a bar outside the band and passes one inside it. A
    /// missing reading cannot refuse: the claim is "RSI is outside the band",
    /// and a warming indicator has not established that.
    #[test]
    fn the_rsi_band_refuses_only_a_reading_it_actually_has() {
        let ts = Utc.with_ymd_and_hms(2026, 7, 1, 15, 0, 0).unwrap();
        let expiry = Utc.with_ymd_and_hms(2026, 8, 21, 20, 0, 0).unwrap();
        let c = cfg(None, Some(70.0));

        let gate = |rsi| entry_gate(0, 0, &c, expiry, ts, None, EntryTiming { rsi, ..Default::default() });
        assert!(matches!(gate(Some(80.0)), EntryGate::RsiOutOfBand));
        assert!(matches!(gate(Some(70.0)), EntryGate::Open), "the bound is inclusive");
        assert!(matches!(gate(Some(45.0)), EntryGate::Open));
        assert!(matches!(gate(None), EntryGate::Open), "a warming indicator refuses nothing");

        // The lower bound is the mirror, and an unset band never fires.
        let lo = cfg(Some(30.0), None);
        assert!(matches!(entry_gate(0, 0, &lo, expiry, ts, None, EntryTiming { rsi: Some(20.0), ..Default::default() }), EntryGate::RsiOutOfBand));
        let off = cfg(None, None);
        assert!(matches!(entry_gate(0, 0, &off, expiry, ts, None, EntryTiming { rsi: Some(99.0), ..Default::default() }), EntryGate::Open));
    }

    /// "Recently crossed up" is a different claim from "is oversold": the level
    /// gate opens while price is still falling, this one waits for the turn.
    #[test]
    fn the_recovery_gate_waits_for_the_turn_and_then_allows_a_window() {
        let ts = Utc.with_ymd_and_hms(2026, 7, 1, 15, 0, 0).unwrap();
        let expiry = Utc.with_ymd_and_hms(2026, 8, 21, 20, 0, 0).unwrap();
        let c = RunConfig {
            max_bars_since_rsi_cross_up: Some(6),
            max_open_positions: 10,
            min_entry_days_before_expiry: None,
            ..RunConfig::default()
        };
        let gate = |n| {
            entry_gate(0, 0, &c, expiry, ts, None, EntryTiming {
                bars_since_rsi_cross_up: n,
                ..Default::default()
            })
        };
        assert!(matches!(gate(Some(0)), EntryGate::Open), "the crossing bar itself");
        assert!(matches!(gate(Some(6)), EntryGate::Open), "the window is inclusive");
        assert!(matches!(gate(Some(7)), EntryGate::NoRecentRsiCross), "stale");
        // Never having crossed is a refusal, unlike a missing RSI reading: "it
        // has not turned up" is established, not unmeasured.
        assert!(matches!(gate(None), EntryGate::NoRecentRsiCross));

        // Unset, the gate refuses nothing at all.
        let off = RunConfig {
            max_open_positions: 10,
            min_entry_days_before_expiry: None,
            ..RunConfig::default()
        };
        assert!(matches!(
            entry_gate(0, 0, &off, expiry, ts, None, EntryTiming::default()),
            EntryGate::Open
        ));
    }

    /// `Outside` is the mirror: it refuses the middle and trades the extremes,
    /// which is the entry an RSI mean-reversion strategy actually wants.
    #[test]
    fn the_outside_zone_trades_the_extremes_and_refuses_the_middle() {
        let ts = Utc.with_ymd_and_hms(2026, 7, 1, 15, 0, 0).unwrap();
        let expiry = Utc.with_ymd_and_hms(2026, 8, 21, 20, 0, 0).unwrap();
        let c = RunConfig {
            min_entry_rsi: Some(30.0),
            max_entry_rsi: Some(70.0),
            entry_rsi_zone: RsiZone::Outside,
            max_open_positions: 10,
            min_entry_days_before_expiry: None,
            ..RunConfig::default()
        };
        let gate = |rsi| entry_gate(0, 0, &c, expiry, ts, None, EntryTiming { rsi, ..Default::default() });
        assert!(matches!(gate(Some(20.0)), EntryGate::Open), "oversold trades");
        assert!(matches!(gate(Some(85.0)), EntryGate::Open), "overbought trades");
        assert!(matches!(gate(Some(50.0)), EntryGate::RsiOutOfBand), "the middle is refused");
        // The bounds themselves are refused, so the two zones partition cleanly.
        assert!(matches!(gate(Some(30.0)), EntryGate::RsiOutOfBand));
        assert!(matches!(gate(Some(70.0)), EntryGate::RsiOutOfBand));
        assert!(matches!(gate(None), EntryGate::Open), "no reading refuses nothing");

        // An unconfigured band cannot refuse in either sense.
        let empty = RunConfig {
            entry_rsi_zone: RsiZone::Outside,
            max_open_positions: 10,
            min_entry_days_before_expiry: None,
            ..RunConfig::default()
        };
        assert!(matches!(entry_gate(0, 0, &empty, expiry, ts, None, EntryTiming { rsi: Some(50.0), ..Default::default() }), EntryGate::Open));
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use crate::portfolio::ExitReason;
    use crate::signal::{MarketView, Signal, Trigger};

    /// A trade holds a release only if it is open when the release's session
    /// opens: closing on the last clean session is getting out in time.
    #[test]
    fn held_through_earnings_means_open_across_the_release() {
        let d = |day| NaiveDate::from_ymd_opt(2026, 7, day).unwrap();
        // After-close on the 29th: its last clean session is the 29th itself.
        let events = [EarningsEvent { date: d(29), close_on: d(29) }];
        assert!(held_across(d(24), d(31), &events));
        assert!(held_across(d(29), d(30), &events), "bought into the print");
        assert!(!held_across(d(24), d(29), &events), "closed before the print");
        assert!(!held_across(d(30), d(31), &events), "opened after it");
    }

    /// Folding a 15-minute chain to 30 keeps the bar that closes on each
    /// 30-minute boundary, relabelled to the 30-minute bar's start: the same
    /// close quote, so the same instant a native 30-minute bar would mark.
    #[test]
    fn a_coarsened_chain_quotes_the_fine_bar_that_closes_with_it() {
        let file = snapshot_with_chains("coarsen", &v_shaped_path(), 100.0, &[expiry()], 15);
        let snapshot = crate::data::Snapshot::open(&file).unwrap();
        let fine = ChainSeries::load(&snapshot, "TEST", expiry()).unwrap();
        let mut coarse = ChainSeries::load(&snapshot, "TEST", expiry()).unwrap();
        coarse.coarsen(30);

        assert_eq!(coarse.bar_minutes, 30);
        assert_eq!(coarse.len(), fine.len() / 2);
        for index in 0..coarse.len() {
            let bar = coarse.snapshot(index).unwrap();
            assert_eq!(bar.ts.timestamp() % 1800, 0, "{}", bar.ts);
            let closing = fine.snapshot_at(bar.ts + Duration::minutes(15)).unwrap();
            assert_eq!(bar.quotes.len(), closing.quotes.len());
            for quote in &bar.quotes {
                let same = closing.quote(quote.contract_id).unwrap();
                assert_eq!((quote.bid, quote.ask), (same.bid, same.ask));
            }
            assert_eq!(bar.underlying.close, closing.underlying.close);
        }
    }

    /// On a 15-minute tape a 30-minute model advances once per window, at the
    /// close of the window's second bar, and decides only there — so every
    /// entry fills on the bar after, which starts on a 30-minute boundary.
    #[test]
    fn a_30_minute_model_on_a_15_minute_tape_decides_once_per_window() {
        let file = snapshot_with_chains("fifteen", &v_shaped_path(), 100.0, &[expiry()], 15);
        let snapshot = crate::data::Snapshot::open(&file).unwrap();
        let series = ChainSeries::load(&snapshot, "TEST", expiry()).unwrap();
        assert_eq!(series.bar_minutes, 15);

        let strategy_config: crate::config::StrategyConfig = toml::from_str(
            r#"
symbols = ["TEST"]
expiry = "2026-08-21"
model = "mean-reversion"
[candidates]
max_loss = 5000.0
[ranking]
min_score = -10.0
min_expected_pl = -1.0e9
[exit]
min_days_before_expiry = 0
"#,
        )
        .unwrap();
        strategy_config.validate().unwrap();
        let mut strategies = vec![strategy_config.strategy().unwrap()];
        let run_config = strategy_config.run_config().unwrap();
        let run = run_shared(&[vec![series]], &mut strategies, &run_config).unwrap();

        let opened: Vec<DateTime<Utc>> = run
            .portfolio
            .closed
            .iter()
            .map(|t| t.opened_at)
            .chain(run.portfolio.open.iter().map(|p| p.opened_at))
            .collect();
        assert!(!opened.is_empty(), "the floors are off, so something opens");
        assert!(
            opened.iter().all(|ts| (ts.timestamp() / 60) % 30 == 0),
            "{opened:?}"
        );
    }

    /// The window is New York time, so it holds across daylight saving: 14:00
    /// UTC is 10:00 in September and 09:00 in December.
    #[test]
    fn the_entry_window_reads_new_york_time() {
        let config = RunConfig {
            entry_window: Some((10 * 60, 15 * 60)),
            ..RunConfig::default()
        };
        let at = |m, d, h, min| chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, m, d, h, min, 0).unwrap();
        assert!(inside_entry_window(&config, at(9, 1, 14, 0)));
        assert!(!inside_entry_window(&config, at(9, 1, 13, 30)), "the opening bar");
        assert!(!inside_entry_window(&config, at(9, 1, 19, 30)), "the last bar");
        assert!(!inside_entry_window(&config, at(12, 1, 14, 0)), "09:00 in winter");
        assert!(inside_entry_window(&RunConfig::default(), at(9, 1, 13, 30)), "no window, no gate");
    }

    #[test]
    fn the_cadence_window_closes_on_its_boundary_only() {
        let at = |h, m| chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 9, 1, h, m, 0).unwrap();
        // 15-minute bars under a 30-minute model: the second bar of each window.
        assert!(!closes_cadence_window(at(13, 30), 15, 30));
        assert!(closes_cadence_window(at(13, 45), 15, 30));
        assert!(closes_cadence_window(at(19, 45), 15, 30), "the session's last window");
        // A tape no finer than the cadence observes every bar, as it always did.
        assert!(closes_cadence_window(at(13, 30), 30, 30));
        assert!(closes_cadence_window(at(20, 0), 30, 30));
        assert!(closes_cadence_window(at(13, 30), 15, 15));
    }

    /// A stock bar that crosses a position's adverse bound queues exactly one
    /// close, for the next option bar; one that does not queues nothing.
    #[test]
    fn a_stock_bar_across_the_stop_queues_one_close() {
        let (_snapshot, series) = series("watch", &v_shaped_path(), 100.0);
        let bar = series.snapshot(0).unwrap();
        let call = bar
            .quotes
            .iter()
            .find(|q| q.right.is_call() && q.strike == 100.0)
            .unwrap();
        let spread = Spread::new(
            "TEST",
            vec![crate::spread::Leg {
                contract_id: call.contract_id,
                strike: call.strike,
                right: call.right,
                side: crate::spread::Side::Long,
                quantity: 1,
            }],
        );
        let mut portfolio = Portfolio::new(10_000.0);
        portfolio.open(spread, &bar, &Costs::default()).unwrap();
        portfolio.open[0].forecast_stop = Some(95.0);

        let exits = ExitRules::default();
        let mut pending = Vec::new();
        watch_stock_bar(&portfolio, "TEST", 96.0, &exits, &mut pending);
        assert!(pending.is_empty(), "above the stop");
        watch_stock_bar(&portfolio, "TEST", 94.0, &exits, &mut pending);
        watch_stock_bar(&portfolio, "TEST", 93.0, &exits, &mut pending);
        assert!(matches!(
            pending.as_slice(),
            [Intent::Close { reason: ExitReason::ForecastStop, .. }]
        ));
    }

    /// Two chains, one symbol: the near one is inside the entry blackout, so
    /// every position must open in the far one — and be marked, exited and
    /// closed only there. Floors are off so the ranker always picks something.
    #[test]
    fn a_position_opens_in_the_chain_that_is_open_and_stays_there() {
        let near = expiry();
        let far = NaiveDate::from_ymd_opt(2026, 9, 18).unwrap();
        let file = snapshot_with_chains("two-chains", &v_shaped_path(), 100.0, &[near, far], 30);
        let snapshot = crate::data::Snapshot::open(&file).unwrap();
        let chains: Vec<ChainSeries> = [near, far]
            .iter()
            .map(|&e| ChainSeries::load(&snapshot, "TEST", e).unwrap())
            .collect();

        let strategy_config: crate::config::StrategyConfig = toml::from_str(
            r#"
symbols = ["TEST"]
expiries = ["2026-08-21", "2026-09-18"]
model = "mean-reversion"
[candidates]
max_loss = 5000.0
[ranking]
min_score = -10.0
min_expected_pl = -1.0e9
[execution]
min_entry_days_before_expiry = 55
"#,
        )
        .unwrap();
        strategy_config.validate().unwrap();
        let mut run_config = strategy_config.run_config().unwrap();
        run_config.max_open_positions = 1;
        let mut strategies = vec![strategy_config.strategy().unwrap()];
        let run = run_shared(&[chains], &mut strategies, &run_config).unwrap();

        let opened: Vec<NaiveDate> = run
            .portfolio
            .closed
            .iter()
            .map(|t| t.expiry)
            .chain(run.portfolio.open.iter().map(|p| p.expiry))
            .collect();
        assert!(!opened.is_empty(), "the far chain offered a trade");
        assert!(opened.iter().all(|&e| e == far), "{opened:?}");
        assert_eq!(run.expiries, vec![near, far]);
        let census = &run.per_symbol[0].census;
        assert_eq!(census.synthetic_exits, 0, "never read against the wrong chain");
        // At most one pick per bar, however many chains offered one.
        assert!(census.ranking.chosen <= v_shaped_path().len(), "{}", census.ranking.chosen);
    }

    /// Both chains open the whole run (the default 8-day blackout never bites
    /// before the near expiry): the front month takes every entry, and only
    /// its ladder is ranked — `best` ranks both.
    #[test]
    fn with_both_chains_open_the_front_month_takes_the_entry() {
        let near = expiry();
        let far = NaiveDate::from_ymd_opt(2026, 9, 18).unwrap();
        let file = snapshot_with_chains("two-chains-open", &v_shaped_path(), 100.0, &[near, far], 30);
        let snapshot = crate::data::Snapshot::open(&file).unwrap();
        let run = |choice: &str| {
            let chains: Vec<ChainSeries> = [near, far]
                .iter()
                .map(|&e| ChainSeries::load(&snapshot, "TEST", e).unwrap())
                .collect();
            let strategy_config: crate::config::StrategyConfig = toml::from_str(&format!(
                r#"
symbols = ["TEST"]
expiries = ["2026-08-21", "2026-09-18"]
model = "mean-reversion"
[candidates]
max_loss = 5000.0
[ranking]
min_score = -10.0
min_expected_pl = -1.0e9
[execution]
expiry_choice = "{choice}"
"#
            ))
            .unwrap();
            strategy_config.validate().unwrap();
            let run_config = strategy_config.run_config().unwrap();
            let mut strategies = vec![strategy_config.strategy().unwrap()];
            run_shared(&[chains], &mut strategies, &run_config).unwrap()
        };
        let front = run("front");
        let best = run("best");

        let opened: Vec<NaiveDate> = front
            .portfolio
            .closed
            .iter()
            .map(|t| t.expiry)
            .chain(front.portfolio.open.iter().map(|p| p.expiry))
            .collect();
        assert!(!opened.is_empty(), "the near chain offered a trade");
        assert!(opened.iter().all(|&e| e == near), "{opened:?}");
        let enumerated = |r: &SharedRun| r.per_symbol[0].census.ranking.enumerated;
        assert!(enumerated(&front) < enumerated(&best), "{} vs {}", enumerated(&front), enumerated(&best));
    }

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
        staged_mut(&mut strategy).signal = Box::new(Silent);
        let run = run_symbol(&series, &mut strategy, &config()).unwrap();

        assert_eq!(run.final_equity, run.initial_cash);
        assert_eq!(run.census.triggers, 0);
        assert!(run.portfolio.closed.is_empty());
        assert!(run.equity_curve.iter().all(|p| p.equity == run.initial_cash));
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
        let last = run.equity_curve.last().unwrap();
        assert_eq!(last.ts, series.expiry_instant());
        assert_eq!(last.equity, run.final_equity);

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
        staged_mut(&mut strategy).signal = Box::new(Always);
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
        staged_mut(&mut strategy).structure = Box::new(Vertical::bear_put(0.50, 0.30, 1).unwrap());
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
        staged_mut(&mut strategy).selector = Box::new(DeltaSelector {
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

    /// The fixture ladder is Black-76 at one vol, so the smile is flat and a
    /// strike struck from the bar — near the money or on a wing, where the smile
    /// is extrapolated — is recovered at its own mid, crossed by its neighbour's
    /// spread.
    #[test]
    fn an_unquoted_strike_is_estimated_off_the_smile() {
        let (_snap, series) = series("estimate", &v_shaped_path(), 120.0);
        let full = series.snapshot(40).unwrap();
        // The far OTM put, a near-money strike, and the far OTM call. (Deep ITM
        // is off by the fixture's 6c floor on its OTM twin, through parity.)
        let last = full.quotes.len() - 1;
        let picks = [
            full.quotes.iter().position(|q| !q.right.is_call()).unwrap(),
            full.quotes.len() / 2,
            full.quotes.iter().rposition(|q| q.right.is_call()).unwrap().min(last),
        ];
        for index in picks {
            let mut bar = full.clone();
            let real = bar.quotes.remove(index);
            let est = bar.estimate_quote(real.contract_id, real.strike, real.right).unwrap();
            assert!((est.mid - real.mid).abs() < 0.02, "{} {:?}", real.strike, (est.mid, real.mid));
            assert!(est.bid <= est.mid && est.mid < est.ask);
        }
    }

    /// When the held strikes stop quoting but the wings still do, the legs are
    /// priced off the bar's smile, so a time-based exit closes on an estimate
    /// with time value rather than at intrinsic.
    #[test]
    fn unquoted_legs_close_on_the_smile_when_a_time_rule_fires() {
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
        assert_eq!(run.census.synthetic_exits, 0, "the smile priced it, not intrinsic");
        assert_eq!(run.portfolio.closed.len(), 1);

        let trade = &run.portfolio.closed[0];
        assert!(trade.estimated_price && !trade.synthetic_price, "flagged as estimated");
        assert_eq!(trade.exit_reason, ExitReason::BeforeExpiry);
    }

    /// When the held strikes stop quoting and the underlying crosses the short
    /// strike, the short-strike rule still fires, priced off the smile.
    #[test]
    fn unquoted_legs_close_at_short_strike_when_underlying_crosses_it() {
        let path = v_shaped_path(); // down 25, up 55 → ends at ~113
        // Dropout at bar 40: ATM quotes vanish. The position opens ~bar 31
        // (RSI crosses 30). By bar 40+ the recovery pushes the underlying
        // past the short call's strike while the held legs are unquoted.
        let file = snapshot_with_dropout("dropout_short_strike", &path, 120.0, 40);
        let snapshot = crate::data::Snapshot::open(&file).unwrap();
        let series = ChainSeries::load(&snapshot, "TEST", expiry()).unwrap();

        let mut strategy = bull_call_strategy();
        let run = run_symbol(
            &series,
            &mut strategy,
            &RunConfig {
                exits: ExitRules {
                    underlying_past_short_strike: true,
                    stop_loss_pct: Some(100.0),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .unwrap();

        assert_eq!(run.census.triggers, 1, "one RSI crossing");
        assert_eq!(run.census.synthetic_exits, 0, "the smile priced it, not intrinsic");
        assert_eq!(run.portfolio.closed.len(), 1);

        let trade = &run.portfolio.closed[0];
        assert!(trade.estimated_price && !trade.synthetic_price, "flagged as estimated");
        assert_eq!(trade.exit_reason, ExitReason::UnderlyingPastShortStrike);
    }
}
