//! Pluggable funding and lot-sizing policies for ranked candidates.

use serde::Serialize;

/// The ranked candidate facts a portfolio policy is allowed to use.
#[derive(Debug, Clone, Copy)]
pub struct AllocationCandidate {
    pub score: f64,
    pub one_lot_max_loss: f64,
    /// ATM IV over its trailing median — the regime the bar is in. `None` while
    /// the feature is still warming, which every policy must treat as "no
    /// opinion" rather than as a neutral 1.0 it did not measure.
    pub iv_vs_median: Option<f64>,
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

/// Size by the *regime* rather than by the score.
///
/// The score is a within-bar comparator (Spearman +0.36) and a poor across-bar
/// one (+0.04), so sizing by it measured no better than flat. ATM IV against its
/// own trailing median is the reverse: it separates realized P/L per unit of risk
/// by +0.27 (debits) and +0.32 (credits) across bars, on 11/12 and 10/12 symbols.
/// This policy therefore ignores the score entirely and scales the budget by the
/// IV regime, clamped so one strange reading cannot take the account with it.
///
/// A bar with no reading gets the base budget — the honest response to "no
/// opinion", and the reason `iv_vs_median` is an `Option` rather than a 1.0.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct IvScaledSizer {
    pub risk_per_position: f64,
    pub max_risk_per_position: f64,
    pub max_lots: Option<u32>,
    pub iv_floor: f64,
    pub iv_cap: f64,
}

impl PortfolioModel for IvScaledSizer {
    fn name(&self) -> &'static str {
        "iv_scaled"
    }

    fn lots(&self, candidate: AllocationCandidate, state: PortfolioState) -> u32 {
        if candidate.one_lot_max_loss <= 0.0 {
            return 1;
        }
        let regime = candidate
            .iv_vs_median
            .map(|r| r.clamp(self.iv_floor, self.iv_cap))
            .unwrap_or(1.0);
        let budget = (state.initial_cash * self.risk_per_position * regime)
            .min(state.initial_cash * self.max_risk_per_position)
            .min(state.remaining_budget);
        let lots = (budget / candidate.one_lot_max_loss).floor() as u32;
        match self.max_lots {
            Some(cap) => lots.min(cap),
            None => lots,
        }
    }
}

/// Runtime dispatch over the built-in portfolio policies. The trait remains the
/// extension seam; this enum keeps `RunConfig` cheap and serializable.
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PortfolioModelKind {
    ScoreWeighted(ScoreWeightedSizer),
    EqualWeight(EqualWeightSizer),
    IvScaled(IvScaledSizer),
}

impl PortfolioModel for PortfolioModelKind {
    fn name(&self) -> &'static str {
        match self {
            Self::ScoreWeighted(model) => model.name(),
            Self::EqualWeight(model) => model.name(),
            Self::IvScaled(model) => model.name(),
        }
    }

    fn lots(&self, candidate: AllocationCandidate, state: PortfolioState) -> u32 {
        match self {
            Self::ScoreWeighted(model) => model.lots(candidate, state),
            Self::EqualWeight(model) => model.lots(candidate, state),
            Self::IvScaled(model) => model.lots(candidate, state),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The regime scales the budget, a missing reading does not, and the clamp
    /// stops one strange print from sizing the account.
    #[test]
    fn the_iv_sizer_scales_by_regime_and_declines_to_guess_without_one() {
        let sizer = IvScaledSizer {
            risk_per_position: 0.025,   // $500 of a $20k account: one lot
            max_risk_per_position: 0.25,
            max_lots: Some(5),
            iv_floor: 0.7,
            iv_cap: 1.5,
        };
        let state = PortfolioState::new(20_000.0, 0.0, 0);
        let lots = |iv| {
            sizer.lots(
                AllocationCandidate { score: 0.05, one_lot_max_loss: 100.0, iv_vs_median: iv },
                state,
            )
        };
        assert_eq!(lots(Some(1.0)), 5, "at the median: $500 budget / $100 = 5 lots");
        assert_eq!(lots(Some(0.7)), 3, "cheap regime sizes down");
        assert_eq!(lots(Some(1.4)), 5, "rich regime sizes up, then meets the lot cap");
        // Clamped: a 10x print cannot buy more than iv_cap allows.
        assert_eq!(lots(Some(10.0)), lots(Some(1.5)));
        // No reading is not a neutral 1.0 by accident — it is the base budget by
        // decision, and it must equal the at-median size.
        assert_eq!(lots(None), lots(Some(1.0)));
    }

    #[test]
    fn score_weighted_sizing_preserves_the_original_budget_math() {
        let model = ScoreWeightedSizer {
            risk_per_position: 0.20,
            max_risk_per_position: 0.50,
            baseline_score: 0.02,
            max_lots: None,
        };
        let candidate = |score| AllocationCandidate {
            iv_vs_median: None,
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
            iv_vs_median: None,
            score: 99.0,
            one_lot_max_loss: 400.0,
        };
        assert_eq!(model.lots(candidate, PortfolioState::new(20_000.0, 0.0, 0)), 3);
        assert_eq!(model.lots(candidate, PortfolioState::new(20_000.0, 19_200.0, 0)), 2);
        assert_eq!(model.lots(candidate, PortfolioState::new(20_000.0, 19_800.0, 0)), 0);
    }
}
