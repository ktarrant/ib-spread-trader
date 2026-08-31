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
