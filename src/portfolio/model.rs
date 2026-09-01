//! Pluggable funding and lot-sizing policies for ranked candidates.

use serde::Serialize;

/// The ranked candidate facts a portfolio policy is allowed to use.
#[derive(Debug, Clone, Copy)]
pub struct AllocationCandidate {
    pub score: f64,
    pub one_lot_max_loss: f64,
}

/// Current account state at fill time. Ordering remains the engine's job; this
/// state lets the policy decide how many lots the next candidate may receive.
#[derive(Debug, Clone, Copy)]
pub struct PortfolioState {
    pub initial_cash: f64,
    pub capital_at_risk: f64,
    pub symbol_open_positions: usize,
    pub remaining_budget: f64,
}

impl PortfolioState {
    pub fn new(initial_cash: f64, capital_at_risk: f64, symbol_open_positions: usize) -> Self {
        Self {
            initial_cash,
            capital_at_risk,
            symbol_open_positions,
            remaining_budget: (initial_cash - capital_at_risk).max(0.0),
        }
    }
}

/// A swappable portfolio allocation policy. Returning zero declines the fill.
pub trait PortfolioModel {
    fn name(&self) -> &'static str;
    fn lots(&self, candidate: AllocationCandidate, state: PortfolioState) -> u32;
}

/// The original conviction-weighted sizing rule.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct ScoreWeightedSizer {
    pub risk_per_position: f64,
    pub max_risk_per_position: f64,
    pub baseline_score: f64,
    pub max_lots: Option<u32>,
}

impl PortfolioModel for ScoreWeightedSizer {
    fn name(&self) -> &'static str {
        "score_weighted"
    }

    fn lots(&self, candidate: AllocationCandidate, state: PortfolioState) -> u32 {
        if candidate.one_lot_max_loss <= 0.0 {
            return 1;
        }
        let score_mult = (candidate.score / self.baseline_score).max(1.0);
        let budget = (state.initial_cash * self.risk_per_position * score_mult)
            .min(state.initial_cash * self.max_risk_per_position)
            .min(state.remaining_budget);
        let lots = (budget / candidate.one_lot_max_loss).floor() as u32;
        match self.max_lots {
            Some(cap) => lots.min(cap),
            None => lots,
        }
    }
}

/// Fixed lots per accepted candidate, still declined when the remaining account
/// budget cannot cover that risk. This proves allocation is independently
/// swappable without changing ranking or engine ordering.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct EqualWeightSizer {
    pub lots: u32,
}

impl PortfolioModel for EqualWeightSizer {
    fn name(&self) -> &'static str {
        "equal_weight"
    }

    fn lots(&self, candidate: AllocationCandidate, state: PortfolioState) -> u32 {
        if candidate.one_lot_max_loss <= 0.0 {
            return self.lots;
        }
        let affordable = (state.remaining_budget / candidate.one_lot_max_loss).floor() as u32;
        self.lots.min(affordable)
    }
}

/// Runtime dispatch over the built-in portfolio policies. The trait remains the
/// extension seam; this enum keeps `RunConfig` cheap and serializable.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PortfolioModelKind {
    ScoreWeighted(ScoreWeightedSizer),
    EqualWeight(EqualWeightSizer),
}

impl PortfolioModel for PortfolioModelKind {
    fn name(&self) -> &'static str {
        match self {
            Self::ScoreWeighted(model) => model.name(),
            Self::EqualWeight(model) => model.name(),
        }
    }

    fn lots(&self, candidate: AllocationCandidate, state: PortfolioState) -> u32 {
        match self {
            Self::ScoreWeighted(model) => model.lots(candidate, state),
            Self::EqualWeight(model) => model.lots(candidate, state),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_weighted_sizing_preserves_the_original_budget_math() {
        let model = ScoreWeightedSizer {
            risk_per_position: 0.20,
            max_risk_per_position: 0.50,
            baseline_score: 0.02,
            max_lots: None,
        };
        let candidate = |score| AllocationCandidate {
            score,
            one_lot_max_loss: 400.0,
        };
        assert_eq!(model.lots(candidate(0.02), PortfolioState::new(20_000.0, 0.0, 0)), 10);
        assert_eq!(model.lots(candidate(0.04), PortfolioState::new(20_000.0, 0.0, 0)), 20);
        assert_eq!(model.lots(candidate(1.0), PortfolioState::new(20_000.0, 0.0, 0)), 25);
        assert_eq!(model.lots(candidate(1.0), PortfolioState::new(20_000.0, 19_800.0, 0)), 0);
    }

    #[test]
    fn equal_weight_is_fixed_but_capital_gated() {
        let model = EqualWeightSizer { lots: 3 };
        let candidate = AllocationCandidate {
            score: 99.0,
            one_lot_max_loss: 400.0,
        };
        assert_eq!(model.lots(candidate, PortfolioState::new(20_000.0, 0.0, 0)), 3);
        assert_eq!(model.lots(candidate, PortfolioState::new(20_000.0, 19_200.0, 0)), 2);
        assert_eq!(model.lots(candidate, PortfolioState::new(20_000.0, 19_800.0, 0)), 0);
    }
}
