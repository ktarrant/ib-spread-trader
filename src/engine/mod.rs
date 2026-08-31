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
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::data::{ChainSeries, ChainSnapshot, EarningsEvent, Quality};
use crate::execution::{Costs, Reject, Selector};
use crate::model::{
    Candidate, CandidateConfig, EarningsDistance, FeatureEngine, Features, ForecastRecord,
    PriceDistribution, PriceModel, RankingConfig, RankingFunnel, ScorerRecord, enumerate_verticals,
    expected_pl_and_score, rank,
};
use crate::portfolio::{ExitReason, Portfolio, TradePrediction};
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
    /// How many spreads may be open at once. One means the strategy is simply
    /// in or out; without a cap a repeatedly-firing signal stacks positions.
    pub max_open_positions: usize,
    /// Refuse new entries when fewer than this many calendar days remain before
    /// expiry. Prevents opening positions the before-expiry exit would
    /// immediately close.
    pub min_entry_days_before_expiry: Option<u32>,
    /// Refuse new entries within this many calendar days of an earnings
    /// announcement. Prevents the same open-close churn as the expiry gate.
    pub earnings_blackout_days: Option<u32>,
    /// Score-weighted position sizing. `None` keeps the fixed one-lot behaviour;
    /// `Some` sizes each ranked entry by conviction and the capital left, so a
    /// limited account fills across opportunities. Ranked path only — the staged
    /// path has no score to size against.
    pub sizing: Option<Sizing>,
}

/// How the ranked path turns a score and the capital left into a contract count.
///
/// A position risks a baseline fraction of total capital at the minimum score,
/// scaled up linearly with the score (conviction buys size) and capped per
/// position. The contract count is that risk budget divided by one lot's max
/// loss, so a wide/expensive structure gets fewer lots than a cheap one for the
/// same budget — capital at risk is what is held constant, not lot count.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Sizing {
    /// Fraction of total capital to risk on a position scoring exactly at the
    /// minimum. Higher scores scale above this, up to `max_risk_per_position`.
    pub risk_per_position: f64,
    /// Ceiling on any single position's risk, as a fraction of total capital, so
    /// one high-scoring idea cannot commit the whole account.
    pub max_risk_per_position: f64,
    /// The score that earns exactly `risk_per_position` — the sizing baseline.
    /// A position scoring `2 * baseline` risks twice as much (before the cap).
    pub baseline_score: f64,
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
            earnings_blackout_days: None,
            sizing: None,
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

/// The universal entry gates, shared by both pipelines: a bar may be refused
/// because a position is already open, or it is too close to expiry or earnings.
enum EntryGate {
    Open,
    PositionLimit,
    TooCloseToExpiry,
    EarningsBlackout,
}

fn entry_gate(
    open_positions: usize,
    exiting: usize,
    config: &RunConfig,
    expiry_instant: DateTime<Utc>,
    bar_ts: DateTime<Utc>,
    next_earnings: Option<&EarningsEvent>,
) -> EntryGate {
    let open_after_exits = open_positions - exiting;
    if open_after_exits >= config.max_open_positions {
        EntryGate::PositionLimit
    } else if config
        .min_entry_days_before_expiry
        .is_some_and(|min_days| (expiry_instant - bar_ts).num_days() < min_days as i64)
    {
        EntryGate::TooCloseToExpiry
    } else if config
        .earnings_blackout_days
        .is_some_and(|days| exits::in_earnings_blackout(bar_ts, next_earnings, days))
    {
        EntryGate::EarningsBlackout
    } else {
        EntryGate::Open
    }
}

/// A decision taken on one bar, to be executed on the next.
#[derive(Debug, Clone)]
enum Intent {
    Open(Spread, Option<TradePrediction>, Option<f64>),
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
    /// Refused because within the earnings blackout window.
    pub earnings_blackout: usize,
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
    pub equity_curve: Vec<(DateTime<Utc>, f64)>,
    /// The underlying print settlement used, if anything was still open.
    pub settlement_price: Option<f64>,
    /// Ranked mode only: one scored forecast per option bar, for model evaluation.
    /// Empty for the staged pipeline.
    pub forecasts: Vec<ForecastRecord>,
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
    let mut scorer_records: Vec<ScorerRecord> = Vec::new();

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
                if let Intent::Open(..) = intent
                    && let Some(position) = portfolio.open.last()
                {
                    opened_at_index.insert(position.id, seq);
                }
            }

            // 2. Mark.
            equity_curve.push((bar.ts, portfolio.equity(&bar, &config.costs)));

            // 3. Exits.
            let next_earnings =
                series.next_earnings_from(bar.ts.date_naive());
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
                            next_earnings,
                        };
                        if let Some(reason) = config.exits.triggered(position, &ctx) {
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
                            .and_then(|spot| {
                                config
                                    .exits
                                    .underlying_price_triggered(&position.spread, spot)
                            })
                            .or_else(|| {
                                config.exits.time_triggered(
                                    position.opened_at,
                                    bar.ts,
                                    series.expiry_instant(),
                                    next_earnings,
                                )
                            });
                        if let Some(reason) = reason {
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
                            ) {
                                EntryGate::PositionLimit => census.at_position_limit += 1,
                                EntryGate::TooCloseToExpiry => census.too_close_to_expiry += 1,
                                EntryGate::EarningsBlackout => census.earnings_blackout += 1,
                                EntryGate::Open => match staged.selector.resolve(&spec, &bar) {
                                    Ok(spread) => {
                                        entering = Some(Intent::Open(spread, None, None))
                                    }
                                    Err(reject) => census.reject(&reject),
                                },
                            },
                        }
                    }
                }
                Strategy::Ranked(ranked) => {
                    // Advance the feature engine every option bar, in order,
                    // whether or not the gate lets us act — its indicators are
                    // stateful, so a skipped bar would corrupt every later reading.
                    let bar_date = bar.ts.date_naive();
                    let earnings = EarningsDistance {
                        days_to: next_earnings.map(|e| (e.date - bar_date).num_days()),
                        days_since: series
                            .previous_earnings_from(bar_date)
                            .map(|e| (bar_date - e.date).num_days()),
                    };
                    let features = ranked.features.observe(&bar, earnings);

                    // Horizon (capped at expiry), the distribution, and the basis
                    // map. The distribution is needed both to log the forecast and
                    // to rank, so it is computed once, on every option bar.
                    let horizon_years = (ranked.ranking.horizon_days / 365.25)
                        .min(features.year_fraction)
                        .max(0.0);
                    let remaining_years = (features.year_fraction - horizon_years).max(0.0);
                    let forward_scale = if features.spot > 0.0 {
                        features.forward / features.spot
                    } else {
                        1.0
                    };
                    let dist = ranked.model.forecast(&features, horizon_years);

                    // Score the forecast against what the underlying actually did a
                    // horizon later. Eval-only — never on the decision path. Uses
                    // the *full* (uncapped) horizon so the realized read lands on a
                    // fixed same-weekday bar; near expiry the target runs past the
                    // window and the bar is simply not scored (see score_forecast).
                    let full_horizon_years = ranked.ranking.horizon_days / 365.25;
                    if let Some(record) = score_forecast(
                        series,
                        &bar,
                        &features,
                        &dist,
                        full_horizon_years,
                        ranked.model.as_ref(),
                    ) {
                        forecasts.push(record);
                    }

                    match entry_gate(
                        portfolio.open.len(),
                        exiting.len(),
                        config,
                        series.expiry_instant(),
                        bar.ts,
                        next_earnings,
                    ) {
                        EntryGate::PositionLimit => census.at_position_limit += 1,
                        EntryGate::TooCloseToExpiry => census.too_close_to_expiry += 1,
                        EntryGate::EarningsBlackout => census.earnings_blackout += 1,
                        EntryGate::Open => {
                            let mut funnel = RankingFunnel::default();
                            let candidates = enumerate_verticals(
                                &bar,
                                &config.costs,
                                &ranked.candidates,
                                &mut funnel,
                            );
                            // Scorer evaluation: sample candidates and price them
                            // forward to grade the score. Reads the candidates
                            // before `rank` consumes them; eval-only.
                            scorer_sample(
                                series,
                                &bar,
                                &candidates,
                                &dist,
                                remaining_years,
                                forward_scale,
                                full_horizon_years,
                                ranked.ranking.forecast_target_quantile,
                                ranked.ranking.scorer_sample_per_bar,
                                &mut scorer_records,
                            );
                            let choice = rank(
                                candidates,
                                &dist,
                                remaining_years,
                                forward_scale,
                                &ranked.ranking,
                                &mut funnel,
                            );
                            census.ranking.merge(&funnel);
                            if let Some(choice) = choice {
                                // Position sizing: turn the score and the capital
                                // left into a lot count. `None` sizing keeps one
                                // lot (fixed behaviour); `Some` scales by
                                // conviction, capped per position and by the
                                // capital still uncommitted.
                                let lots = match config.sizing {
                                    None => 1,
                                    Some(sizing) => {
                                        let one_lot_risk = choice.candidate.max_loss;
                                        let available = config.initial_cash
                                            - portfolio.capital_at_risk();
                                        let score_mult =
                                            (choice.score / sizing.baseline_score).max(1.0);
                                        let budget = (config.initial_cash
                                            * sizing.risk_per_position
                                            * score_mult)
                                            .min(
                                                config.initial_cash
                                                    * sizing.max_risk_per_position,
                                            )
                                            .min(available.max(0.0));
                                        (budget / one_lot_risk).floor() as u32
                                    }
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
                                    // The forecast target: the favourable quantile
                                    // of the entry distribution, in the trade's
                                    // direction. A price, so size does not touch it.
                                    let forecast_target =
                                        ranked.ranking.forecast_target_quantile.map(|q| {
                                            let level = if choice
                                                .candidate
                                                .spread
                                                .profits_from_a_rise()
                                            {
                                                q
                                            } else {
                                                1.0 - q
                                            };
                                            dist.quantile(level)
                                        });
                                    let spread = if lots == 1 {
                                        choice.candidate.spread
                                    } else {
                                        choice.candidate.spread.scaled(lots)
                                    };
                                    entering = Some(Intent::Open(
                                        spread,
                                        Some(prediction),
                                        forecast_target,
                                    ));
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
                        if let Intent::Open(..) = intent
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
        equity_curve.push((series.expiry_instant(), portfolio.cash));
    }

    let final_equity = portfolio.cash;
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
        scorer_records,
    })
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
    dist: &PriceDistribution,
    remaining_years: f64,
    forward_scale: f64,
    full_horizon_years: f64,
    forecast_target_quantile: Option<f64>,
    sample_per_bar: usize,
    out: &mut Vec<ScorerRecord>,
) {
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
        let (expected_pl, score) = expected_pl_and_score(
            candidate,
            dist,
            remaining_years,
            forward_scale,
            forecast_target_quantile,
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
            score,
            expected_pl,
            realized_pl,
            max_loss: candidate.max_loss,
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

    // The random-walk null: same model, but features that carry no drift and let
    // the vol fall back to ATM IV.
    let null_features = Features {
        trend: None,
        vwap_distance: None,
        skew: None,
        realized_vol: None,
        atr: None,
        ..*features
    };
    let null = model.forecast(&null_features, horizon_years);

    Some(ForecastRecord::score(
        bar.ts,
        &series.symbol,
        features.spot,
        features.forward,
        features.atm_iv,
        horizon_years,
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
        Intent::Open(spread, prediction, forecast_target) => {
            // The bar that arrives may not quote what the previous one did. A
            // missed fill is a real outcome, counted rather than forced.
            match portfolio.open(spread.clone(), bar, &config.costs) {
                Ok(_) => {
                    if let Some(position) = portfolio.open.last_mut() {
                        position.prediction = *prediction;
                        position.forecast_target = *forecast_target;
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
                 kind TEXT NOT NULL, source TEXT NOT NULL, source_id TEXT, title TEXT);
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
             CREATE TABLE IF NOT EXISTS events (id INTEGER PRIMARY KEY, symbol TEXT,
                 event_date TEXT NOT NULL, event_time TEXT, session_timing TEXT,
                 kind TEXT NOT NULL, source TEXT NOT NULL, source_id TEXT, title TEXT);
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
        staged_mut(&mut strategy).signal = Box::new(Silent);
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

    /// When option quotes disappear but the underlying crosses the short
    /// strike, the engine should close at intrinsic via the short-strike
    /// rule — not wait for a time-based exit or ride to expiry.
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
        assert_eq!(
            run.census.synthetic_exits, 1,
            "closed at intrinsic when underlying crossed short strike"
        );
        assert_eq!(run.portfolio.closed.len(), 1);

        let trade = &run.portfolio.closed[0];
        assert!(trade.synthetic_price, "flagged as synthetic");
        assert_eq!(trade.exit_reason, ExitReason::UnderlyingPastShortStrike);
    }
}
