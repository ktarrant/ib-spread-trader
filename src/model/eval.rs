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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn dist(prices: Vec<f64>, weights: Vec<f64>) -> PriceDistribution {
        PriceDistribution::from_weights(prices, weights).unwrap()
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
