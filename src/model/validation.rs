//! Auditable validation records for labeled forecast and ranking blocks.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A forecast must beat its random-walk null, not merely match it.
pub const MIN_CRPS_SKILL: f64 = 0.0;
/// Direction must beat a coin flip on the dense forecast sample.
pub const MIN_DIRECTIONAL_HIT_RATE: f64 = 0.5;
/// Total-variation distance of the PIT histogram from uniform. This deliberately
/// allows modest sampling noise while rejecting visibly miscalibrated forecasts.
pub const MAX_PIT_CALIBRATION_ERROR: f64 = 0.15;
/// At least two thirds of symbols must beat the directional null independently.
pub const MIN_LOSO_PASS_FRACTION: f64 = 0.66;

/// The evidence attached to a labeled price model on one dataset.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationRecord {
    pub label: String,
    pub dataset: String,
    pub horizon_days: f64,
    pub crps_skill: f64,
    pub pit_error: f64,
    pub coverage_50: f64,
    pub coverage_80: f64,
    pub coverage_90: f64,
    pub directional_hit_rate: f64,
    pub loso_min_dir_hit: f64,
    pub loso_symbols_passing: usize,
    pub loso_symbols_total: usize,
    pub passed: bool,
    pub evaluated_at: DateTime<Utc>,
}

impl ValidationRecord {
    /// Recompute the gate from the evidence rather than trusting serialized
    /// `passed`, which makes a checked-in record auditable after thresholds move.
    pub fn passes_gate(&self) -> bool {
        let required_loso =
            (MIN_LOSO_PASS_FRACTION * self.loso_symbols_total as f64).ceil() as usize;
        self.crps_skill > MIN_CRPS_SKILL
            && self.directional_hit_rate > MIN_DIRECTIONAL_HIT_RATE
            && self.pit_error < MAX_PIT_CALIBRATION_ERROR
            && self.loso_symbols_total > 0
            && self.loso_symbols_passing >= required_loso
    }

    pub fn refresh_passed(&mut self) {
        self.passed = self.passes_gate();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn passing_record() -> ValidationRecord {
        ValidationRecord {
            label: "mean-reversion".into(),
            dataset: "2026-08-21".into(),
            horizon_days: 7.0,
            crps_skill: 0.01,
            pit_error: 0.10,
            coverage_50: 0.50,
            coverage_80: 0.80,
            coverage_90: 0.90,
            directional_hit_rate: 0.57,
            loso_min_dir_hit: 0.40,
            loso_symbols_passing: 8,
            loso_symbols_total: 12,
            passed: false,
            evaluated_at: Utc::now(),
        }
    }

    #[test]
    fn the_forecast_gate_requires_null_beating_calibrated_loso_evidence() {
        let record = passing_record();
        assert!(record.passes_gate(), "8/12 clears ceil(0.66 * 12)");

        let mut failed = record.clone();
        failed.crps_skill = 0.0;
        assert!(!failed.passes_gate(), "matching the null is not enough");

        let mut failed = record.clone();
        failed.directional_hit_rate = 0.5;
        assert!(!failed.passes_gate(), "a coin flip is not directional skill");

        let mut failed = record.clone();
        failed.pit_error = MAX_PIT_CALIBRATION_ERROR;
        assert!(!failed.passes_gate(), "the PIT threshold is strict");

        let mut failed = record;
        failed.loso_symbols_passing = 7;
        assert!(!failed.passes_gate(), "7/12 does not clear the LOSO gate");
    }
}
