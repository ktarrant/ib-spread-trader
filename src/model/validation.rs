//! Auditable validation records for labeled forecast and ranking blocks.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// A forecast must beat its random-walk null, not merely match it — and the gate
/// applies this to the **lower bound** of the weekly-block interval, not the
/// point estimate, so a skill within noise of zero does not pass.
pub const MIN_CRPS_SKILL: f64 = 0.0;
/// Direction must beat a coin flip on the dense forecast sample.
pub const MIN_DIRECTIONAL_HIT_RATE: f64 = 0.5;
/// Total-variation distance of the PIT histogram from uniform. This deliberately
/// allows modest sampling noise while rejecting visibly miscalibrated forecasts.
pub const MAX_PIT_CALIBRATION_ERROR: f64 = 0.15;
/// At least two thirds of symbols must beat the directional null independently.
pub const MIN_LOSO_PASS_FRACTION: f64 = 0.66;
/// Decile means need a clearly positive overall slope while tolerating noisy
/// adjacent reversals in this short dataset.
pub const MIN_DECILE_MONOTONICITY: f64 = 0.5;

/// The evidence attached to a labeled price model on one dataset. Checked in at
/// `models/<label>/<dataset>.json`, one per dataset, so a second dataset adds
/// evidence rather than overwriting the first.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationRecord {
    pub label: String,
    pub dataset: String,
    pub horizon_days: f64,
    pub crps_skill: f64,
    /// 95 % weekly-block bootstrap interval on `crps_skill`; see `ModelEval`.
    pub crps_skill_interval: [f64; 2],
    /// Weekly blocks behind the interval.
    pub weeks: usize,
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
        self.crps_skill_interval[0] > MIN_CRPS_SKILL
            && self.directional_hit_rate > MIN_DIRECTIONAL_HIT_RATE
            && self.pit_error < MAX_PIT_CALIBRATION_ERROR
            && self.loso_symbols_total > 0
            && self.loso_symbols_passing >= required_loso
    }

    pub fn refresh_passed(&mut self) {
        self.passed = self.passes_gate();
    }
}

/// Scorer discrimination is meaningful only for the model whose forecast it
/// ranked, so every ranker record names that model explicitly. Checked in at
/// `rankers/<label>/<dataset>.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RankerValidationRecord {
    pub label: String,
    pub model_label: String,
    pub dataset: String,
    pub spearman: f64,
    pub decile_monotonicity: f64,
    pub passed: bool,
    pub evaluated_at: DateTime<Utc>,
}

impl RankerValidationRecord {
    pub fn passes_gate(&self) -> bool {
        self.spearman > 0.0 && self.decile_monotonicity >= MIN_DECILE_MONOTONICITY
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
            crps_skill_interval: [0.002, 0.018],
            weeks: 10,
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
        failed.crps_skill_interval[0] = -0.001;
        assert!(!failed.passes_gate(), "a skill within noise of zero is not enough");

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

    /// The Python drivers write the records, so their copy of the gate could
    /// drift from this one and nothing would notice — a registry of records that
    /// say `passed` under thresholds nobody applies any more. Recomputing every
    /// checked-in record here makes that drift a test failure instead.
    #[test]
    fn every_checked_in_record_still_earns_its_verdict() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let models: usize = records(&root.join("models"))
            .map(|(label, dataset, text)| {
                let record: ValidationRecord = serde_json::from_str(&text)
                    .unwrap_or_else(|err| panic!("{label}/{dataset}: {err}"));
                assert_eq!((record.label.as_str(), record.dataset.as_str()), (&*label, &*dataset));
                assert_eq!(
                    record.passed,
                    record.passes_gate(),
                    "{label}/{dataset} records a verdict the gate does not reproduce"
                );
            })
            .count();
        assert!(models >= 2, "expected the model registry, found {models}");

        let rankers: usize = records(&root.join("rankers"))
            .map(|(label, dataset, text)| {
                let record: RankerValidationRecord = serde_json::from_str(&text)
                    .unwrap_or_else(|err| panic!("{label}/{dataset}: {err}"));
                assert_eq!((record.label.as_str(), record.dataset.as_str()), (&*label, &*dataset));
                assert_eq!(
                    record.passed,
                    record.passes_gate(),
                    "{label}/{dataset} records a verdict the gate does not reproduce"
                );
            })
            .count();
        assert!(rankers >= 1, "expected the ranker registry, found {rankers}");
    }

    /// Every `<registry>/<label>/<dataset>.json`, as (label, dataset, text).
    fn records(registry: &std::path::Path) -> impl Iterator<Item = (String, String, String)> {
        let dirs = std::fs::read_dir(registry).unwrap().map(|e| e.unwrap().path());
        dirs.filter(|dir| dir.is_dir()).flat_map(|dir| {
            let label = dir.file_name().unwrap().to_string_lossy().into_owned();
            std::fs::read_dir(&dir)
                .unwrap()
                .map(|e| e.unwrap().path())
                .filter(|p| p.extension().is_some_and(|ext| ext == "json"))
                .map(move |p| {
                    let dataset = p.file_stem().unwrap().to_string_lossy().into_owned();
                    (label.clone(), dataset, std::fs::read_to_string(&p).unwrap())
                })
                .collect::<Vec<_>>()
        })
    }

    /// The mean-reversion drift was labeled on a point estimate of +0.0133. Read
    /// with its weekly-block interval it does not pass on either month, and the
    /// random-walk null never could — the gate's worked example, both ways.
    #[test]
    fn the_directional_model_does_not_clear_the_interval_gate() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("models");
        for (label, dataset) in [
            ("mean-reversion", "2026-08-21"),
            ("mean-reversion", "2026-10-16"),
            ("driftless", "2026-08-21"),
        ] {
            let path = root.join(label).join(format!("{dataset}.json"));
            let text = std::fs::read_to_string(&path).unwrap();
            let record: ValidationRecord = serde_json::from_str(&text).unwrap();
            assert!(!record.passes_gate(), "{label}/{dataset}");
        }
        let august = std::fs::read_to_string(root.join("mean-reversion/2026-08-21.json")).unwrap();
        let august: ValidationRecord = serde_json::from_str(&august).unwrap();
        assert!(august.crps_skill > 0.0 && august.crps_skill_interval[0] < 0.0);
    }

    #[test]
    fn the_ranker_gate_needs_positive_candidate_and_decile_ordering() {
        let record = RankerValidationRecord {
            label: "ev".into(),
            model_label: "mean-reversion".into(),
            dataset: "2026-08-21".into(),
            spearman: 0.37,
            decile_monotonicity: 0.75,
            passed: false,
            evaluated_at: Utc::now(),
        };
        assert!(record.passes_gate());

        let mut failed = record.clone();
        failed.spearman = 0.0;
        assert!(!failed.passes_gate());

        let mut failed = record;
        failed.decile_monotonicity = 0.49;
        assert!(!failed.passes_gate());
    }
}
