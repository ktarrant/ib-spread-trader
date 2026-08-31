//! Ranking the candidates by expected P/L per unit of risk.
//!
//! Each candidate is marked to model at the scoring horizon against the price
//! distribution: its expected value is `Σ pᵢ · model_value(sᵢ · forward_scale,
//! remaining, leg_vols)`, and its expected P/L is that less the cost basis. The
//! score is expected P/L over max loss — capital at risk — with a positive-
//! expectancy floor and a minimum score. The best surviving candidate is taken.
//!
//! Risk in the denominator is the **structural** max loss (held to expiry), not
//! the interim worst case: it is the capital genuinely committed, and it does not
//! move with the horizon.

use super::candidates::{Candidate, RankingFunnel};
use super::distribution::PriceDistribution;

/// What the ranker maximises when it picks among the candidates that clear the
/// floors. Both read the same de-biased mark-to-model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RankMetric {
    /// Expected P/L per unit of risk — the default. Rewards magnitude of edge.
    ExpectedValue,
    /// Probability of a net profit at the horizon. Rewards hit rate, and tends
    /// toward wide, high-odds spreads with a poor payoff when they lose — the
    /// classic "pick up pennies" profile, which the evaluation makes visible.
    ProbProfit,
}

/// The horizon and thresholds the ranker applies. Mapped from TOML in a later
/// phase.
#[derive(Debug, Clone, Copy)]
pub struct RankingConfig {
    /// Scoring horizon in calendar days, capped at expiry by the engine.
    pub horizon_days: f64,
    /// What to maximise among candidates that clear the floors.
    pub metric: RankMetric,
    /// Minimum expected-P/L-per-max-loss to open a trade.
    pub min_score: f64,
    /// Positive-expectancy floor, in dollars: expected P/L must exceed this.
    pub min_expected_pl: f64,
    /// If set, close a position when the underlying reaches this quantile of the
    /// entry forecast in the trade's favour (e.g. 0.85 = the 85th percentile of
    /// the up-move for a bullish trade). Grounds the exit on the same forecast the
    /// entry used. `None` disables it.
    pub forecast_target_quantile: Option<f64>,
    /// Evaluation only: how many candidates per bar to sample and price forward
    /// to the horizon, to test whether the score actually predicts realized P/L.
    /// `0` disables it (the default), so normal runs pay nothing.
    pub scorer_sample_per_bar: usize,
}

impl Default for RankingConfig {
    fn default() -> Self {
        Self {
            // One calendar week: a forecast made on a given weekday is scored
            // against the same weekday next week, which is always a trading bar.
            // Shorter horizons (e.g. 5 days) land Monday/Tuesday forecasts on the
            // weekend, where the realized read collapses to the prior Friday.
            horizon_days: 7.0,
            metric: RankMetric::ExpectedValue,
            min_score: 0.10,
            min_expected_pl: 0.0,
            forecast_target_quantile: None,
            scorer_sample_per_bar: 0,
        }
    }
}

/// The chosen candidate and why it won.
#[derive(Debug, Clone)]
pub struct RankedChoice {
    pub candidate: Candidate,
    pub expected_pl: f64,
    pub score: f64,
    /// Model probability the position is worth more than its cost basis at the
    /// horizon — a diagnostic, not a gate.
    pub prob_profit: f64,
}

/// Score every candidate and return the best that clears the floors, recording
/// the funnel. `remaining_years` is the time from the horizon to expiry (zero
/// scores at expiry); `forward_scale` maps a sampled spot to its forward,
/// preserving the current parity basis.
/// The model's expected P/L and score (`E[P/L] / max_loss`) for one candidate.
/// Shared by `rank` and the scorer evaluation so the number the ranker acts on
/// and the number the evaluation grades can never drift apart.
pub fn expected_pl_and_score(
    candidate: &Candidate,
    dist: &PriceDistribution,
    remaining_years: f64,
    forward_scale: f64,
) -> (f64, f64) {
    let expected_value = dist.expected(|s| {
        candidate
            .spread
            .model_value(s * forward_scale, remaining_years, &candidate.leg_vols)
    });
    // The model value is a mid mark; a real exit crosses the book, so charge the
    // exit spread here rather than let the score read optimistically.
    let expected_pl = expected_value - candidate.cost_basis - candidate.exit_spread_cost;
    (expected_pl, expected_pl / candidate.max_loss)
}

/// Model probability the candidate is worth more than its cost basis at the
/// horizon. Its own pass over the grid, computed only for a candidate worth
/// keeping.
fn prob_profit(
    candidate: &Candidate,
    dist: &PriceDistribution,
    remaining_years: f64,
    forward_scale: f64,
) -> f64 {
    // Profit means clearing the cost basis *and* the exit crossing — the same
    // net breakeven the expected P/L is measured against.
    let breakeven = candidate.cost_basis + candidate.exit_spread_cost;
    dist.expected(|s| {
        let value =
            candidate
                .spread
                .model_value(s * forward_scale, remaining_years, &candidate.leg_vols);
        f64::from(value > breakeven)
    })
}

pub fn rank(
    candidates: Vec<Candidate>,
    dist: &PriceDistribution,
    remaining_years: f64,
    forward_scale: f64,
    cfg: &RankingConfig,
    funnel: &mut RankingFunnel,
) -> Option<RankedChoice> {
    let mut best: Option<RankedChoice> = None;

    for candidate in candidates {
        let (expected_pl, score) =
            expected_pl_and_score(&candidate, dist, remaining_years, forward_scale);
        if expected_pl <= cfg.min_expected_pl {
            continue;
        }
        funnel.positive_ev += 1;

        if score < cfg.min_score {
            continue;
        }
        funnel.above_min_score += 1;

        let prob_profit = prob_profit(&candidate, dist, remaining_years, forward_scale);

        let choice = RankedChoice {
            candidate,
            expected_pl,
            score,
            prob_profit,
        };
        // Rank by the configured metric; break ties on the other so a field of
        // equal-probability spreads still resolves to the highest-edge one.
        let key = |c: &RankedChoice| match cfg.metric {
            RankMetric::ExpectedValue => (c.score, c.prob_profit),
            RankMetric::ProbProfit => (c.prob_profit, c.score),
        };
        let better = match &best {
            None => true,
            Some(current) => key(&choice) > key(current),
        };
        if better {
            best = Some(choice);
        }
    }

    if best.is_some() {
        funnel.chosen += 1;
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::OptionRight;
    use crate::spread::{Leg, Side, Spread};

    /// A 100/105 bull call debit spread priced at $1.90 (cost basis $192.60 with
    /// the round-trip commission), long 100C / short 105C.
    fn bull_call() -> Candidate {
        let spread = Spread::new(
            "TEST",
            vec![
                Leg {
                    contract_id: 1,
                    strike: 100.0,
                    right: OptionRight::Call,
                    side: Side::Long,
                    quantity: 1,
                },
                Leg {
                    contract_id: 2,
                    strike: 105.0,
                    right: OptionRight::Call,
                    side: Side::Short,
                    quantity: 1,
                },
            ],
        );
        Candidate {
            spread,
            entry_debit: 190.0,
            cost_basis: 192.6,
            max_profit: Some(500.0 - 192.6),
            max_loss: 192.6,
            leg_vols: vec![0.2, 0.2],
            rel_spread: 0.05,
            exit_spread_cost: 0.0,
        }
    }

    /// Mass concentrated above the spread's strikes: 0.2 at 100, 0.3 at 105,
    /// 0.5 at 110. Scored at expiry (`remaining_years = 0`), the spread's model
    /// value is its intrinsic payoff.
    fn bullish_distribution() -> PriceDistribution {
        PriceDistribution::from_weights(vec![100.0, 105.0, 110.0], vec![2.0, 3.0, 5.0]).unwrap()
    }

    #[test]
    fn a_profitable_spread_is_chosen_with_the_hand_scored_pl() {
        let mut funnel = RankingFunnel::default();
        let choice = rank(
            vec![bull_call()],
            &bullish_distribution(),
            0.0,
            1.0,
            &RankingConfig::default(),
            &mut funnel,
        )
        .expect("a profitable spread is taken");

        // E[payoff] = 0.3*500 + 0.5*500 = 400; expected P/L = 400 - 192.6.
        assert!((choice.expected_pl - 207.4).abs() < 1e-9, "{}", choice.expected_pl);
        assert!((choice.score - 207.4 / 192.6).abs() < 1e-9);
        assert!((choice.prob_profit - 0.8).abs() < 1e-9, "{}", choice.prob_profit);
        assert_eq!(funnel.chosen, 1);
    }

    #[test]
    fn a_min_score_above_the_edge_takes_nothing() {
        let mut funnel = RankingFunnel::default();
        let cfg = RankingConfig {
            min_score: 5.0,
            ..RankingConfig::default()
        };
        let choice = rank(
            vec![bull_call()],
            &bullish_distribution(),
            0.0,
            1.0,
            &cfg,
            &mut funnel,
        );
        assert!(choice.is_none());
        assert_eq!(funnel.positive_ev, 1, "it was profitable, just not enough");
        assert_eq!(funnel.above_min_score, 0);
        assert_eq!(funnel.chosen, 0);
    }

    #[test]
    fn a_bearish_distribution_gives_the_bull_call_negative_expectancy() {
        let mut funnel = RankingFunnel::default();
        // Almost all mass below the long strike: the debit is mostly lost.
        let bearish =
            PriceDistribution::from_weights(vec![90.0, 95.0, 100.0], vec![5.0, 4.0, 1.0]).unwrap();
        let choice = rank(
            vec![bull_call()],
            &bearish,
            0.0,
            1.0,
            &RankingConfig::default(),
            &mut funnel,
        );
        assert!(choice.is_none());
        assert_eq!(funnel.positive_ev, 0);
    }

    #[test]
    fn the_higher_scoring_candidate_wins() {
        let mut funnel = RankingFunnel::default();
        // A second candidate identical but with a smaller max loss scores higher
        // on the same expected P/L.
        let mut tighter = bull_call();
        tighter.max_loss = 96.3;
        tighter.max_profit = Some(500.0 - 96.3);

        let choice = rank(
            vec![bull_call(), tighter],
            &bullish_distribution(),
            0.0,
            1.0,
            &RankingConfig::default(),
            &mut funnel,
        )
        .unwrap();
        assert!((choice.candidate.max_loss - 96.3).abs() < 1e-9, "the tighter one");
        assert_eq!(funnel.chosen, 1);
        assert_eq!(funnel.above_min_score, 2);
    }
}
