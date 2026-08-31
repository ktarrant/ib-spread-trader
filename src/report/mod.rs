//! Aggregating runs into a result, and saying what the result cannot support.
//!
//! **There is no Sharpe ratio here, on purpose.** The reference snapshot gives
//! one expiry, ~44 sessions, and a handful of trades per symbol. A Sharpe
//! computed on that would be a number with no standard error worth quoting, and
//! its only real function would be to make the result look more settled than it
//! is. What the data does support is per-trade detail, so that is what gets
//! reported — alongside a `caveats` list the report carries about itself.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::collections::BTreeMap;
use std::path::Path;

use crate::engine::SymbolRun;
use crate::model::{ForecastRecord, ModelEval, RankingFunnel};
use crate::portfolio::{ClosedTrade, ExitReason};

#[derive(Debug, Clone, Default, Serialize)]
pub struct Census {
    pub triggers: usize,
    pub declined: usize,
    pub at_position_limit: usize,
    pub unfillable_intents: usize,
    pub synthetic_exits: usize,
    pub too_close_to_expiry: usize,
    pub earnings_blackout: usize,
    pub signal_diagnostics: BTreeMap<String, usize>,
    pub rejections: BTreeMap<String, usize>,
    pub rejection_examples: BTreeMap<String, String>,
    /// The ranked path's candidate funnel, summed across symbols. All zero for a
    /// staged run.
    pub ranking: RankingFunnel,
}

#[derive(Debug, Clone, Serialize)]
pub struct SymbolSummary {
    pub symbol: String,
    pub bars: usize,
    pub trades: usize,
    pub pnl: f64,
    pub traded_pnl: f64,
    pub settled_pnl: f64,
    pub max_drawdown: f64,
    pub triggers: usize,
    pub rejections: usize,
    pub signal_diagnostics: BTreeMap<String, usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Totals {
    pub symbols: usize,
    pub bars: usize,
    pub capital_deployed: f64,
    pub final_equity: f64,
    pub total_pnl: f64,
    pub return_pct: f64,
    /// P&L from positions closed on a real quote.
    pub traded_pnl: f64,
    /// P&L from positions the tape never priced an exit for — settled at
    /// intrinsic because the option bars stop before expiry.
    pub settled_pnl: f64,
    pub commission_paid: f64,
    pub trades: usize,
    pub wins: usize,
    pub win_rate: Option<f64>,
    pub average_trade_pnl: Option<f64>,
    /// Gross wins over gross losses. `None` with no losing trades — a profit
    /// factor of "infinity" on a handful of trades would flatter the result.
    pub profit_factor: Option<f64>,
    /// Mean realized `pnl / max_loss` — return per dollar of capital at risk, the
    /// guardrail the ranked score is supposed to maximise. `None` if no trade had
    /// a bounded max loss.
    pub avg_pnl_per_risk: Option<f64>,
    pub best_trade: Option<f64>,
    pub worst_trade: Option<f64>,
    pub exits_by_reason: BTreeMap<String, usize>,
    /// On the combined curve, equal capital per symbol.
    pub max_drawdown: f64,
    pub census: Census,
}

/// One symbol's forecast-quality metrics.
#[derive(Debug, Clone, Serialize)]
pub struct SymbolModelEval {
    pub symbol: String,
    #[serde(flatten)]
    pub eval: ModelEval,
}

/// Forecast-quality evaluation for a ranked run: pooled across every scored bar,
/// and broken out per symbol. `None` for a staged run, which forecasts nothing.
#[derive(Debug, Clone, Serialize)]
pub struct ModelEvalReport {
    pub pooled: ModelEval,
    pub per_symbol: Vec<SymbolModelEval>,
}

impl ModelEvalReport {
    fn build(runs: &[SymbolRun]) -> Option<Self> {
        let pooled_records: Vec<ForecastRecord> =
            runs.iter().flat_map(|r| r.forecasts.iter().cloned()).collect();
        let pooled = ModelEval::from_records(&pooled_records)?;
        let per_symbol = runs
            .iter()
            .filter_map(|run| {
                ModelEval::from_records(&run.forecasts).map(|eval| SymbolModelEval {
                    symbol: run.symbol.clone(),
                    eval,
                })
            })
            .collect();
        Some(Self { pooled, per_symbol })
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub strategy: String,
    pub expiry: chrono::NaiveDate,
    pub totals: Totals,
    pub symbols: Vec<SymbolSummary>,
    pub trades: Vec<ClosedTrade>,
    /// Forecast-quality metrics for a ranked run; absent for a staged one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_eval: Option<ModelEvalReport>,
    /// What this result does and does not support. Part of the output, not a
    /// footnote — the numbers above are easy to over-read without it.
    pub caveats: Vec<String>,
}

pub fn build(strategy: &str, expiry: chrono::NaiveDate, runs: &[SymbolRun]) -> Report {
    let mut census = Census::default();
    let mut symbols = Vec::new();
    let mut trades: Vec<ClosedTrade> = Vec::new();
    let mut exits_by_reason: BTreeMap<String, usize> = BTreeMap::new();

    for run in runs {
        let (traded, settled) = run.portfolio.realized_split();
        symbols.push(SymbolSummary {
            symbol: run.symbol.clone(),
            bars: run.bars,
            trades: run.portfolio.closed.len(),
            pnl: run.final_equity - run.initial_cash,
            traded_pnl: traded,
            settled_pnl: settled,
            max_drawdown: max_drawdown(&run.equity_curve),
            triggers: run.census.triggers,
            rejections: run.census.total_rejections(),
            signal_diagnostics: run.signal_diagnostics.clone(),
        });

        census.triggers += run.census.triggers;
        census.declined += run.census.declined;
        census.at_position_limit += run.census.at_position_limit;
        census.unfillable_intents += run.census.unfillable_intents;
        census.synthetic_exits += run.census.synthetic_exits;
        census.too_close_to_expiry += run.census.too_close_to_expiry;
        census.earnings_blackout += run.census.earnings_blackout;
        census.ranking.merge(&run.census.ranking);
        for (gate, count) in &run.signal_diagnostics {
            *census.signal_diagnostics.entry(gate.clone()).or_default() += count;
        }
        for (kind, count) in &run.census.rejections {
            *census.rejections.entry(kind.clone()).or_default() += count;
        }
        for (kind, example) in &run.census.rejection_examples {
            census
                .rejection_examples
                .entry(kind.clone())
                .or_insert_with(|| example.clone());
        }

        for trade in &run.portfolio.closed {
            *exits_by_reason
                .entry(reason_key(trade.exit_reason).to_string())
                .or_default() += 1;
        }
        trades.extend(run.portfolio.closed.iter().cloned());
    }

    trades.sort_by_key(|trade| trade.closed_at);

    let capital_deployed: f64 = runs.iter().map(|run| run.initial_cash).sum();
    let final_equity: f64 = runs.iter().map(|run| run.final_equity).sum();
    let total_pnl = final_equity - capital_deployed;
    let traded_pnl: f64 = symbols.iter().map(|s| s.traded_pnl).sum();
    let settled_pnl: f64 = symbols.iter().map(|s| s.settled_pnl).sum();
    let commission_paid: f64 = trades.iter().map(|t| t.commission).sum();
    let wins = trades.iter().filter(|t| t.pnl > 0.0).count();
    let pnls: Vec<f64> = trades.iter().map(|t| t.pnl).collect();

    let gross_win: f64 = pnls.iter().filter(|p| **p > 0.0).sum();
    let gross_loss: f64 = pnls.iter().filter(|p| **p < 0.0).map(|p| p.abs()).sum();
    let profit_factor = (gross_loss > 0.0).then(|| gross_win / gross_loss);

    let risk_ratios: Vec<f64> = trades
        .iter()
        .filter_map(|t| t.max_loss.filter(|ml| *ml > 0.0).map(|ml| t.pnl / ml))
        .collect();
    let avg_pnl_per_risk =
        (!risk_ratios.is_empty()).then(|| risk_ratios.iter().sum::<f64>() / risk_ratios.len() as f64);

    let totals = Totals {
        symbols: runs.len(),
        bars: runs.iter().map(|run| run.bars).sum(),
        capital_deployed,
        final_equity,
        total_pnl,
        return_pct: if capital_deployed > 0.0 {
            100.0 * total_pnl / capital_deployed
        } else {
            0.0
        },
        traded_pnl,
        settled_pnl,
        commission_paid,
        trades: trades.len(),
        wins,
        win_rate: (!trades.is_empty()).then(|| 100.0 * wins as f64 / trades.len() as f64),
        average_trade_pnl: (!pnls.is_empty()).then(|| pnls.iter().sum::<f64>() / pnls.len() as f64),
        profit_factor,
        avg_pnl_per_risk,
        best_trade: pnls.iter().copied().reduce(f64::max),
        worst_trade: pnls.iter().copied().reduce(f64::min),
        exits_by_reason,
        max_drawdown: max_drawdown(&combined_curve(runs)),
        census,
    };

    let model_eval = ModelEvalReport::build(runs);
    let caveats = caveats(&totals, runs, model_eval.as_ref());
    Report {
        strategy: strategy.to_string(),
        expiry,
        totals,
        symbols,
        trades,
        model_eval,
        caveats,
    }
}

/// What the numbers above cannot support, stated with them rather than after.
fn caveats(totals: &Totals, runs: &[SymbolRun], model_eval: Option<&ModelEvalReport>) -> Vec<String> {
    let mut out = vec![
        "No Sharpe or annualised return is reported: one expiry and ~44 sessions \
         cannot support one, and quoting it would make the result look more settled \
         than it is."
            .to_string(),
    ];

    // The null-beating gate: a forecast worse than a random walk is not a better
    // forecaster, whatever P&L it happened to produce. A small band around zero
    // reads as "matches the null" rather than the alarm — a driftless model is
    // essentially the null, which is honest, not broken.
    if let Some(eval) = model_eval {
        let skill = eval.pooled.crps_skill;
        let cal = format!(
            "mean PIT {:.2}, 50/90% coverage {:.2}/{:.2}",
            eval.pooled.pit_mean, eval.pooled.coverage_50, eval.pooled.coverage_90
        );
        if skill < -0.02 {
            out.push(format!(
                "The price model forecasts WORSE than a random-walk null (CRPS skill \
                 {skill:+.2}): its drift and vol are miscalibrated, so the ranked P&L \
                 below rests on a forecast that has not earned trust. Tune the model \
                 before reading its edge."
            ));
        } else if skill <= 0.02 {
            out.push(format!(
                "The price model matches a random-walk null on CRPS (skill {skill:+.2}); \
                 {cal}. It is calibrated but carries no directional edge over a random \
                 walk — the ranked P&L is a vol/liquidity result, not a forecast one."
            ));
        } else {
            out.push(format!(
                "The price model beats a random-walk null on CRPS by {:.0}% (skill \
                 {skill:+.2}); {cal}.",
                100.0 * skill,
            ));
        }
    }

    if totals.trades < 30 {
        out.push(format!(
            "{} trades is too few for the win rate to be more than descriptive.",
            totals.trades
        ));
    }
    if totals.settled_pnl != 0.0 {
        out.push(format!(
            "${:.2} of P&L came from expiry settlement at intrinsic, not from a \
             quoted exit — the option tape stops the day before expiry.",
            totals.settled_pnl
        ));
    }
    let rejections = totals.census.rejections.values().sum::<usize>();
    if rejections > totals.trades && rejections > 0 {
        out.push(format!(
            "{rejections} rejections against {} fills: the ladder could not supply \
             most of what this strategy asked for.",
            totals.trades
        ));
    }
    let funnel = &totals.census.ranking;
    if funnel.enumerated > 0 {
        out.push(
            "This is a model-driven result: the ranking rests on the price model's \
             assumptions — the vol blend, the drift weights, and sticky-strike interim \
             marks — none of which are validated against realised outcomes here."
                .to_string(),
        );
        if funnel.chosen == 0 {
            out.push(format!(
                "The model priced {} candidate vertical(s) but the expectancy and score \
                 floors took none: this result is those floors, not the model's edge.",
                funnel.enumerated
            ));
        }
    }
    if totals.trades > 0 && totals.commission_paid.abs() > totals.total_pnl.abs() {
        out.push(format!(
            "${:.2} of commission against ${:.2} of P&L — costs dominate this result.",
            totals.commission_paid, totals.total_pnl
        ));
    }
    if totals.census.synthetic_exits > 0 {
        let n = totals.census.synthetic_exits;
        out.push(format!(
            "{n} exit{} priced at intrinsic because option quotes were unavailable — \
             the P&L on {} trade{} is an estimate, not a market observation.",
            if n == 1 { " was" } else { "s were" },
            if n == 1 { "that" } else { "those" },
            if n == 1 { "" } else { "s" },
        ));
    }
    let untraded: Vec<&str> = runs
        .iter()
        .filter(|run| run.portfolio.closed.is_empty())
        .map(|run| run.symbol.as_str())
        .collect();
    if !untraded.is_empty() {
        out.push(format!(
            "{} of {} symbols never traded: {}.",
            untraded.len(),
            runs.len(),
            untraded.join(", ")
        ));
    }
    out
}

fn reason_key(reason: ExitReason) -> &'static str {
    match reason {
        ExitReason::ProfitTarget => "profit_target",
        ExitReason::StopLoss => "stop_loss",
        ExitReason::UnderlyingPastShortStrike => "underlying_past_short",
        ExitReason::TimeDecay => "time_decay",
        ExitReason::BeforeEarnings => "before_earnings",
        ExitReason::BeforeExpiry => "before_expiry",
        ExitReason::Expiry => "expiry_settlement",
    }
}

/// Every symbol's equity summed on the union of their timestamps, each carried
/// forward from its last print. Equal capital per symbol, held from the start —
/// so the combined curve starts at the sum of the initial cash.
pub fn combined_curve(runs: &[SymbolRun]) -> Vec<(DateTime<Utc>, f64)> {
    let mut stamps: Vec<DateTime<Utc>> = runs
        .iter()
        .flat_map(|run| run.equity_curve.iter().map(|&(ts, _)| ts))
        .collect();
    stamps.sort_unstable();
    stamps.dedup();

    let mut cursors = vec![0usize; runs.len()];
    let mut last: Vec<f64> = runs.iter().map(|run| run.initial_cash).collect();
    let mut out = Vec::with_capacity(stamps.len());

    for stamp in stamps {
        for (i, run) in runs.iter().enumerate() {
            while cursors[i] < run.equity_curve.len() && run.equity_curve[cursors[i]].0 <= stamp {
                last[i] = run.equity_curve[cursors[i]].1;
                cursors[i] += 1;
            }
        }
        out.push((stamp, last.iter().sum()));
    }
    out
}

pub fn max_drawdown(curve: &[(DateTime<Utc>, f64)]) -> f64 {
    let mut peak = f64::NEG_INFINITY;
    let mut worst: f64 = 0.0;
    for &(_, equity) in curve {
        peak = peak.max(equity);
        if peak > 0.0 {
            worst = worst.max((peak - equity) / peak);
        }
    }
    worst
}

/// Write the trade log and the combined equity curve beside the JSON summary.
pub fn write_csvs(dir: &Path, report: &Report, runs: &[SymbolRun]) -> Result<()> {
    std::fs::create_dir_all(dir)
        .with_context(|| format!("creating output directory {}", dir.display()))?;

    let mut trades = String::from(
        "id,symbol,description,opened_at,closed_at,bars_held,entry_debit,exit_credit,commission,pnl,exit_reason\n",
    );
    for t in &report.trades {
        trades.push_str(&format!(
            "{},{},\"{}\",{},{},{},{:.2},{:.2},{:.2},{:.2},{}\n",
            t.id,
            t.symbol,
            t.description,
            t.opened_at.to_rfc3339(),
            t.closed_at.to_rfc3339(),
            t.bars_held,
            t.entry_debit,
            t.exit_credit,
            t.commission,
            t.pnl,
            reason_key(t.exit_reason),
        ));
    }
    std::fs::write(dir.join("trades.csv"), trades)?;

    let mut equity = String::from("ts,equity\n");
    for (ts, value) in combined_curve(runs) {
        equity.push_str(&format!("{},{:.2}\n", ts.to_rfc3339(), value));
    }
    std::fs::write(dir.join("equity.csv"), equity)?;

    // Ranked-mode evaluation logs. Written only when the data exists, so a staged
    // run's output directory is byte-identical to before.
    let has_forecasts = runs.iter().any(|r| !r.forecasts.is_empty());
    if has_forecasts {
        let mut forecasts = String::from(
            "ts,symbol,spot,forward,atm_iv,horizon_years,mean,stddev,\
             q05,q10,q25,q50,q75,q90,q95,realized,model_crps,null_crps,pit\n",
        );
        for run in runs {
            for f in &run.forecasts {
                let atm_iv = f.atm_iv.map(|v| format!("{v:.6}")).unwrap_or_default();
                forecasts.push_str(&format!(
                    "{},{},{:.4},{:.4},{},{:.6},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.4},{:.6}\n",
                    f.ts.to_rfc3339(), f.symbol, f.spot, f.forward, atm_iv, f.horizon_years,
                    f.mean, f.stddev,
                    f.quantiles[0], f.quantiles[1], f.quantiles[2], f.quantiles[3],
                    f.quantiles[4], f.quantiles[5], f.quantiles[6],
                    f.realized, f.model_crps, f.null_crps, f.pit,
                ));
            }
        }
        std::fs::write(dir.join("forecasts.csv"), forecasts)?;
    }

    let has_predictions = report.trades.iter().any(|t| t.prediction.is_some());
    if has_predictions {
        let mut predictions =
            String::from("id,symbol,description,expected_pl,score,prob_profit,pnl,exit_reason\n");
        for t in &report.trades {
            if let Some(p) = t.prediction {
                predictions.push_str(&format!(
                    "{},{},\"{}\",{:.2},{:.4},{:.4},{:.2},{}\n",
                    t.id,
                    t.symbol,
                    t.description,
                    p.expected_pl,
                    p.score,
                    p.prob_profit,
                    t.pnl,
                    reason_key(t.exit_reason),
                ));
            }
        }
        std::fs::write(dir.join("predictions.csv"), predictions)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn ts(hour: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 1, hour, 0, 0).unwrap()
    }

    #[test]
    fn drawdown_measures_the_deepest_fall_from_a_peak() {
        let curve = vec![
            (ts(13), 100.0),
            (ts(14), 120.0),
            (ts(15), 90.0),
            (ts(16), 110.0),
        ];
        assert!((max_drawdown(&curve) - 0.25).abs() < 1e-12);
    }

    #[test]
    fn a_curve_that_only_rises_has_no_drawdown() {
        let curve = vec![(ts(13), 100.0), (ts(14), 110.0), (ts(15), 120.0)];
        assert_eq!(max_drawdown(&curve), 0.0);
        assert_eq!(max_drawdown(&[]), 0.0);
    }

    /// Symbols print on different timestamps, so the combined curve has to carry
    /// each forward from its last print — not drop it, and not restart it.
    #[test]
    fn the_combined_curve_carries_each_symbol_forward() {
        use crate::data::Quality;
        use crate::engine::{Census as EngineCensus, SymbolRun};
        use crate::portfolio::Portfolio;

        let run = |symbol: &str, points: Vec<(DateTime<Utc>, f64)>| SymbolRun {
            symbol: symbol.into(),
            strategy: "t".into(),
            expiry: chrono::NaiveDate::from_ymd_opt(2026, 8, 21).unwrap(),
            bars: points.len(),
            first_ts: points.first().map(|p| p.0),
            last_ts: points.last().map(|p| p.0),
            initial_cash: 100.0,
            final_equity: points.last().map(|p| p.1).unwrap_or(100.0),
            census: EngineCensus::default(),
            signal_diagnostics: BTreeMap::new(),
            option_quality: Quality::default(),
            underlying_quality: Quality::default(),
            portfolio: Portfolio::new(100.0),
            equity_curve: points,
            settlement_price: None,
            forecasts: Vec::new(),
        };

        let runs = vec![
            run("A", vec![(ts(13), 100.0), (ts(15), 120.0)]),
            run("B", vec![(ts(14), 90.0)]),
        ];
        let curve = combined_curve(&runs);

        assert_eq!(curve.len(), 3, "the union of both symbols' timestamps");
        assert_eq!(curve[0], (ts(13), 200.0), "B still at its initial cash");
        assert_eq!(curve[1], (ts(14), 190.0), "A carried forward at 100");
        assert_eq!(curve[2], (ts(15), 210.0), "B carried forward at 90");
    }
}
