//! Per-bar forecast records, for judging the price model against what actually
//! happened.
//!
//! The strategy never sees any of this — a record joins each bar's forecast to
//! the underlying's **realized** price a horizon later, which is future
//! information. It is logged for evaluation only and is never fed back into a
//! trade decision, so it cannot leak look-ahead into a result. The engine reads
//! the realized price from the already-loaded series precisely because that read
//! is safe here and nowhere near the decision path.

use chrono::{DateTime, Utc};
use serde::Serialize;

use super::distribution::PriceDistribution;

/// The quantile levels every record carries, so a coverage table can be built
/// downstream: the 50% interval is `[q25, q75]`, 80% is `[q10, q90]`, 90% is
/// `[q05, q95]`.
pub const QUANTILE_LEVELS: [f64; 7] = [0.05, 0.10, 0.25, 0.50, 0.75, 0.90, 0.95];

/// One option bar's forecast, scored against the realized underlying at the
/// horizon. Everything needed for calibration, sharpness, and CRPS-skill lives
/// here; the report and the viz aggregate across records.
#[derive(Debug, Clone, Serialize)]
pub struct ForecastRecord {
    pub ts: DateTime<Utc>,
    pub symbol: String,
    pub spot: f64,
    pub forward: f64,
    pub atm_iv: Option<f64>,
    pub horizon_years: f64,
    pub mean: f64,
    pub stddev: f64,
    /// Forecast quantiles at [`QUANTILE_LEVELS`].
    pub quantiles: [f64; 7],
    /// Underlying price a horizon after the bar (or expiry settlement).
    pub realized: f64,
    /// CRPS of the model's forecast against `realized` (lower is better).
    pub model_crps: f64,
    /// CRPS of a random-walk null (driftless, vol = ATM IV) against `realized`.
    /// The model beats the null when `model_crps < null_crps`.
    pub null_crps: f64,
    /// What the model saw on this bar. Carried so a candidate regime — realized
    /// vol against implied, a trend state, an IV level — can be tested against
    /// forecast quality on 6 000 bars instead of 50 trades, without a new run.
    pub features: crate::model::EntryFeatures,
    /// Probability integral transform, `F_model(realized)`. Uniform(0,1) across
    /// bars iff the model is calibrated.
    pub pit: f64,
}

impl ForecastRecord {
    /// Score a bar's model and null forecasts against the realized outcome.
    #[allow(clippy::too_many_arguments)]
    pub fn score(
        ts: DateTime<Utc>,
        symbol: &str,
        spot: f64,
        forward: f64,
        atm_iv: Option<f64>,
        horizon_years: f64,
        features: crate::model::EntryFeatures,
        model: &PriceDistribution,
        null: &PriceDistribution,
        realized: f64,
    ) -> Self {
        let mut quantiles = [0.0; 7];
        for (slot, &level) in quantiles.iter_mut().zip(&QUANTILE_LEVELS) {
            *slot = model.quantile(level);
        }
        Self {
            ts,
            symbol: symbol.to_string(),
            spot,
            forward,
            atm_iv,
            horizon_years,
            features,
            mean: model.mean(),
            stddev: model.stddev(),
            quantiles,
            realized,
            model_crps: model.crps(realized),
            null_crps: null.crps(realized),
            pit: model.cdf(realized),
        }
    }

    /// Whether `realized` fell inside the central interval of coverage `level`
    /// (e.g. 0.50 → the `[q25, q75]` band). `None` if `level` is not one of the
    /// supported symmetric bands.
    pub fn covers(&self, level: f64) -> Option<bool> {
        let (lo, hi) = match level {
            l if (l - 0.50).abs() < 1e-9 => (self.quantiles[2], self.quantiles[4]),
            l if (l - 0.80).abs() < 1e-9 => (self.quantiles[1], self.quantiles[5]),
            l if (l - 0.90).abs() < 1e-9 => (self.quantiles[0], self.quantiles[6]),
            _ => return None,
        };
        Some(lo <= self.realized && self.realized <= hi)
    }
}

/// Aggregate forecast-quality metrics over a set of records — the answer to "is
/// the forecast any good?", judged on the dense per-bar data rather than the
/// sparse trades.
#[derive(Debug, Clone, Serialize)]
pub struct ModelEval {
    pub forecasts: usize,
    /// Mean CRPS of the model and of the random-walk null. Lower is better.
    pub crps_model: f64,
    pub crps_null: f64,
    /// `1 − crps_model/crps_null`: positive iff the model beats the null. The
    /// headline number — the null-beating gate the tuning is judged against.
    pub crps_skill: f64,
    /// Fraction of outcomes that fell inside the central 50/80/90% intervals. A
    /// calibrated model matches the nominal level (0.50/0.80/0.90).
    pub coverage_50: f64,
    pub coverage_80: f64,
    pub coverage_90: f64,
    /// Mean PIT (≈0.5 when unbiased) and its histogram over ten bins (flat when
    /// calibrated), with a total-variation distance from uniform (0 = perfect).
    pub pit_mean: f64,
    pub pit_histogram: [usize; 10],
    pub pit_calibration_error: f64,
    /// Mean predictive stddev — sharpness. Only meaningful read next to coverage.
    pub mean_stddev: f64,
    /// Fraction of bars where the sign of the model's drift matched the realized
    /// move. 0.5 is a coin flip.
    pub directional_hit_rate: f64,
}

impl ModelEval {
    /// `None` for an empty set (no scored forecasts, e.g. a staged run).
    pub fn from_records(records: &[ForecastRecord]) -> Option<Self> {
        if records.is_empty() {
            return None;
        }
        let n = records.len() as f64;

        let crps_model = records.iter().map(|r| r.model_crps).sum::<f64>() / n;
        let crps_null = records.iter().map(|r| r.null_crps).sum::<f64>() / n;
        let crps_skill = if crps_null > 0.0 {
            1.0 - crps_model / crps_null
        } else {
            0.0
        };

        let coverage = |level: f64| {
            records.iter().filter(|r| r.covers(level) == Some(true)).count() as f64 / n
        };

        let pit_mean = records.iter().map(|r| r.pit).sum::<f64>() / n;
        let mut pit_histogram = [0usize; 10];
        for r in records {
            let bin = ((r.pit * 10.0).floor() as usize).min(9);
            pit_histogram[bin] += 1;
        }
        // Total-variation distance of the PIT histogram from uniform.
        let pit_calibration_error = 0.5
            * pit_histogram
                .iter()
                .map(|&c| (c as f64 / n - 0.1).abs())
                .sum::<f64>();

        let mean_stddev = records.iter().map(|r| r.stddev).sum::<f64>() / n;

        let (mut directional_hits, mut directional_total) = (0usize, 0usize);
        for r in records {
            let predicted = r.mean - r.spot;
            let realized = r.realized - r.spot;
            if predicted != 0.0 && realized != 0.0 {
                directional_total += 1;
                if predicted.signum() == realized.signum() {
                    directional_hits += 1;
                }
            }
        }
        let directional_hit_rate = if directional_total > 0 {
            directional_hits as f64 / directional_total as f64
        } else {
            0.0
        };

        Some(Self {
            forecasts: records.len(),
            crps_model,
            crps_null,
            crps_skill,
            coverage_50: coverage(0.50),
            coverage_80: coverage(0.80),
            coverage_90: coverage(0.90),
            pit_mean,
            pit_histogram,
            pit_calibration_error,
            mean_stddev,
            directional_hit_rate,
        })
    }
}

/// One sampled candidate, priced forward to the horizon, for the scorer test:
/// does the model's `score` actually predict the realized P/L?
#[derive(Debug, Clone, Serialize)]
pub struct ScorerRecord {
    pub ts: DateTime<Utc>,
    pub symbol: String,
    /// The value the ranker ordered by: `E[P/L] / max_loss` for the
    /// expected-value metric, the model probability of profit for `prob_profit`.
    /// Grading the ordering key is what makes the Spearman a claim about *this*
    /// ranker rather than about the default one.
    pub score: f64,
    /// The model's expected P/L, in dollars.
    pub expected_pl: f64,
    /// What the candidate actually made, opened now and closed on the tape a
    /// horizon later (or at intrinsic when a leg stopped quoting).
    pub realized_pl: f64,
    pub max_loss: f64,
    /// Signed entry cost and leg count — enough to name the structure
    /// (`legs == 1` is a naked long, else the sign of `entry_debit` gives debit
    /// or credit), so outcomes can be split by structure without re-running.
    pub entry_debit: f64,
    pub legs: u8,
    /// The model's probability this candidate clears its round-trip cost at the
    /// horizon. Recorded beside the realized outcome so the *probability* can be
    /// calibrated, which the trade log cannot do on ~50 samples.
    pub prob_profit: f64,
}

/// One score-decile's realized outcome.
#[derive(Debug, Clone, Serialize)]
pub struct DecileStat {
    /// 1 (lowest score) … 10 (highest).
    pub decile: usize,
    pub count: usize,
    pub mean_score: f64,
    pub mean_realized_pl: f64,
    pub mean_realized_pl_per_risk: f64,
}

/// Whether the ranker's score discriminates: realized P/L bucketed by score
/// decile, plus the Spearman rank correlation between score and realized P/L. A
/// good scorer shows realized P/L rising across the deciles and a positive rho.
#[derive(Debug, Clone, Serialize)]
pub struct ScorerEval {
    pub records: usize,
    /// Spearman rank correlation of score vs realized P/L (−1..1).
    pub spearman: f64,
    /// Mean realized P/L per dollar of risk across all sampled candidates — the
    /// null a decile has to beat to be "better than average".
    pub mean_realized_pl_per_risk: f64,
    /// Spearman correlation of decile number vs mean realized P/L. Positive is
    /// the scalar form of "higher score buckets generally realized more."
    pub decile_monotonicity: f64,
    pub deciles: Vec<DecileStat>,
}

impl ScorerEval {
    /// `None` unless there are at least ten records — fewer cannot fill deciles.
    pub fn from_records(records: &[ScorerRecord]) -> Option<Self> {
        if records.len() < 10 {
            return None;
        }
        let n = records.len();

        let scores: Vec<f64> = records.iter().map(|r| r.score).collect();
        let realized: Vec<f64> = records.iter().map(|r| r.realized_pl).collect();
        let spearman = pearson(&average_ranks(&scores), &average_ranks(&realized));

        let mean_realized_pl_per_risk =
            records.iter().map(|r| r.realized_pl / r.max_loss).sum::<f64>() / n as f64;

        // Sort a view by score, then cut into ten contiguous buckets.
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by(|&a, &b| records[a].score.total_cmp(&records[b].score));

        let deciles: Vec<DecileStat> = (0..10)
            .map(|d| {
                let slice = &order[d * n / 10..(d + 1) * n / 10];
                let count = slice.len().max(1);
                let mean = |f: &dyn Fn(&ScorerRecord) -> f64| {
                    slice.iter().map(|&i| f(&records[i])).sum::<f64>() / count as f64
                };
                DecileStat {
                    decile: d + 1,
                    count: slice.len(),
                    mean_score: mean(&|r| r.score),
                    mean_realized_pl: mean(&|r| r.realized_pl),
                    mean_realized_pl_per_risk: mean(&|r| r.realized_pl / r.max_loss),
                }
            })
            .collect();
        let decile_monotonicity = decile_monotonicity(&deciles);

        Some(Self {
            records: n,
            spearman,
            mean_realized_pl_per_risk,
            decile_monotonicity,
            deciles,
        })
    }
}

/// Spearman correlation of score-decile index with that decile's mean realized
/// P/L. Ties are handled exactly like the candidate-level scorer correlation.
pub fn decile_monotonicity(deciles: &[DecileStat]) -> f64 {
    if deciles.len() < 2 {
        return 0.0;
    }
    let indices: Vec<f64> = deciles.iter().map(|d| d.decile as f64).collect();
    let realized: Vec<f64> = deciles.iter().map(|d| d.mean_realized_pl).collect();
    pearson(&average_ranks(&indices), &average_ranks(&realized))
}

/// Fractional ranks with ties averaged, so Spearman is exact under ties.
fn average_ranks(values: &[f64]) -> Vec<f64> {
    let mut order: Vec<usize> = (0..values.len()).collect();
    order.sort_by(|&a, &b| values[a].total_cmp(&values[b]));
    let mut ranks = vec![0.0; values.len()];
    let mut i = 0;
    while i < order.len() {
        let mut j = i + 1;
        while j < order.len() && values[order[j]] == values[order[i]] {
            j += 1;
        }
        // Average rank (1-based) for the tied group order[i..j].
        let avg = ((i + 1 + j) as f64) / 2.0;
        for &idx in &order[i..j] {
            ranks[idx] = avg;
        }
        i = j;
    }
    ranks
}

fn pearson(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len() as f64;
    let (mean_a, mean_b) = (a.iter().sum::<f64>() / n, b.iter().sum::<f64>() / n);
    let mut cov = 0.0;
    let (mut var_a, mut var_b) = (0.0, 0.0);
    for (&x, &y) in a.iter().zip(b) {
        let (dx, dy) = (x - mean_a, y - mean_b);
        cov += dx * dy;
        var_a += dx * dx;
        var_b += dy * dy;
    }
    if var_a > 0.0 && var_b > 0.0 {
        cov / (var_a.sqrt() * var_b.sqrt())
    } else {
        0.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn dist(prices: Vec<f64>, weights: Vec<f64>) -> PriceDistribution {
        PriceDistribution::from_weights(prices, weights).unwrap()
    }

    fn features(spot: f64) -> crate::model::EntryFeatures {
        crate::model::EntryFeatures {
            spot,
            trend: None,
            vwap_distance: None,
            atr: None,
            realized_vol: None,
            atm_iv: None,
            skew: None,
            rsi: None,
            days_to_earnings: None,
        }
    }

    #[test]
    fn a_record_scores_model_against_null_and_reads_pit() {
        // Model straddles the outcome symmetrically; null sits far below it.
        let model = dist(vec![98.0, 102.0], vec![1.0, 1.0]);
        let null = dist(vec![90.0, 91.0], vec![1.0, 1.0]);
        let r = ForecastRecord::score(
            Utc.with_ymd_and_hms(2026, 8, 3, 15, 0, 0).unwrap(),
            "TEST",
            100.0,
            100.0,
            Some(0.2),
            0.02,
            features(100.0),
            &model,
            &null,
            100.0,
        );
        assert!(r.model_crps < r.null_crps, "the sharper model wins on CRPS");
        // Half the mass sits strictly below the outcome → PIT ½.
        assert!((r.pit - 0.5).abs() < 1e-9, "{}", r.pit);
    }

    #[test]
    fn aggregate_metrics_summarize_a_record_set() {
        assert!(ModelEval::from_records(&[]).is_none());

        // Model predicts up (mean 102 > spot 100) and sits near the outcome; the
        // null is far below.
        let model = dist(vec![100.0, 104.0], vec![1.0, 1.0]);
        let null = dist(vec![80.0, 81.0], vec![1.0, 1.0]);
        let mk = |realized| {
            ForecastRecord::score(
                Utc.with_ymd_and_hms(2026, 8, 3, 15, 0, 0).unwrap(),
                "T",
                100.0,
                100.0,
                Some(0.2),
                0.02,
                features(100.0),
                &model,
                &null,
                realized,
            )
        };
        let eval = ModelEval::from_records(&[mk(103.0), mk(101.0)]).unwrap();
        assert_eq!(eval.forecasts, 2);
        assert!(eval.crps_skill > 0.5, "model beats the far null: {}", eval.crps_skill);
        assert_eq!(eval.coverage_50, 1.0, "both realized inside [q25,q75]");
        assert_eq!(eval.directional_hit_rate, 1.0, "both moved up, as predicted");
    }

    fn scorer_records(realized: impl Fn(usize) -> f64) -> Vec<ScorerRecord> {
        let ts = Utc.with_ymd_and_hms(2026, 8, 3, 15, 0, 0).unwrap();
        (0..20)
            .map(|i| ScorerRecord {
                ts,
                symbol: "T".into(),
                score: i as f64 * 0.1,
                expected_pl: i as f64,
                realized_pl: realized(i),
                max_loss: 100.0,
                entry_debit: 100.0,
                legs: 2,
                prob_profit: 0.5,
            })
            .collect()
    }

    #[test]
    fn scorer_eval_detects_a_discriminating_score() {
        // Realized P/L rises with score → Spearman 1 and increasing deciles.
        let eval = ScorerEval::from_records(&scorer_records(|i| i as f64 * 2.0)).unwrap();
        assert!((eval.spearman - 1.0).abs() < 1e-9, "{}", eval.spearman);
        assert!((eval.decile_monotonicity - 1.0).abs() < 1e-9);
        assert_eq!(eval.deciles.len(), 10);
        assert!(
            eval.deciles.last().unwrap().mean_realized_pl
                > eval.deciles.first().unwrap().mean_realized_pl
        );
    }

    #[test]
    fn scorer_eval_flags_a_score_that_predicts_the_wrong_way() {
        let eval = ScorerEval::from_records(&scorer_records(|i| -(i as f64))).unwrap();
        assert!(eval.spearman < -0.9, "{}", eval.spearman);
    }

    #[test]
    fn scorer_eval_needs_enough_records_to_fill_deciles() {
        assert!(ScorerEval::from_records(&scorer_records(|i| i as f64)[..9]).is_none());
    }

    #[test]
    fn coverage_reads_the_symmetric_bands() {
        let model = dist(
            vec![80.0, 90.0, 100.0, 110.0, 120.0],
            vec![1.0, 1.0, 1.0, 1.0, 1.0],
        );
        let record = |realized| {
            ForecastRecord::score(
                Utc.with_ymd_and_hms(2026, 8, 3, 15, 0, 0).unwrap(),
                "TEST",
                100.0,
                100.0,
                None,
                0.02,
                features(100.0),
                &model,
                &model,
                realized,
            )
        };
        // q25=90, q75=110 for this flat five-point grid.
        assert_eq!(record(100.0).covers(0.50), Some(true));
        assert_eq!(record(115.0).covers(0.50), Some(false));
        assert_eq!(record(115.0).covers(0.90), Some(true));
        assert_eq!(record(100.0).covers(0.42), None);
    }
}
