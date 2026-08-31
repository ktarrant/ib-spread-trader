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

/// The horizon and thresholds the ranker applies. Mapped from TOML in a later
/// phase.
#[derive(Debug, Clone, Copy)]
pub struct RankingConfig {
    /// Scoring horizon in calendar days, capped at expiry by the engine.
    pub horizon_days: f64,
    /// Minimum expected-P/L-per-max-loss to open a trade.
    pub min_score: f64,
    /// Positive-expectancy floor, in dollars: expected P/L must exceed this.
    pub min_expected_pl: f64,
}

impl Default for RankingConfig {
    fn default() -> Self {
        Self {
            // One calendar week: a forecast made on a given weekday is scored
            // against the same weekday next week, which is always a trading bar.
            // Shorter horizons (e.g. 5 days) land Monday/Tuesday forecasts on the
            // weekend, where the realized read collapses to the prior Friday.
            horizon_days: 7.0,
            min_score: 0.10,
            min_expected_pl: 0.0,
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
        let expected_value = dist.expected(|s| {
            candidate
                .spread
                .model_value(s * forward_scale, remaining_years, &candidate.leg_vols)
        });
        let expected_pl = expected_value - candidate.cost_basis;
        if expected_pl <= cfg.min_expected_pl {
            continue;
        }
        funnel.positive_ev += 1;

        let score = expected_pl / candidate.max_loss;
        if score < cfg.min_score {
            continue;
        }
        funnel.above_min_score += 1;

        let prob_profit = dist.expected(|s| {
            let value =
                candidate
                    .spread
                    .model_value(s * forward_scale, remaining_years, &candidate.leg_vols);
            f64::from(value > candidate.cost_basis)
        });

        let choice = RankedChoice {
            candidate,
            expected_pl,
            score,
            prob_profit,
        };
        let better = match &best {
            None => true,
            Some(current) => choice.score > current.score,
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
            max_profit: 500.0 - 192.6,
            max_loss: 192.6,
            leg_vols: vec![0.2, 0.2],
            rel_spread: 0.05,
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
        tighter.max_profit = 500.0 - 96.3;

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
