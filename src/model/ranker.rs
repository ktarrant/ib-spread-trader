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

/// The thresholds the ranker applies. Mapped from TOML in a later phase.
#[derive(Debug, Clone, Copy)]
pub struct RankingConfig {
    /// What to maximise among candidates that clear the floors.
    pub metric: RankMetric,
    /// Minimum expected-P/L-per-max-loss to open a trade.
    pub min_score: f64,
    /// Positive-expectancy floor, in dollars: expected P/L must exceed this.
    pub min_expected_pl: f64,
    /// Entry gate on the model probability of profit: refuse a candidate whose
    /// chance of clearing its round-trip cost is below this. `None` disables it.
    ///
    /// Distinct from ranking *by* probability. `metric = "prob_profit"` takes the
    /// best odds on offer however poor they are; this refuses to open at all
    /// below a floor, which is how a short-premium book is actually run — the
    /// thesis is "this expires worthless", and a 55 % chance of that is not that
    /// thesis. It is also independent of `min_score`, which floors expected value
    /// per unit of risk: a candidate can clear the edge floor on a fat payoff it
    /// rarely collects, and this refuses it.
    pub min_prob_profit: Option<f64>,
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
            metric: RankMetric::ExpectedValue,
            min_score: 0.10,
            min_expected_pl: 0.0,
            min_prob_profit: None,
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
    forecast_target_quantile: Option<f64>,
) -> (f64, f64) {
    let value_at = |s: f64| {
        candidate
            .spread
            .model_value(s * forward_scale, remaining_years, &candidate.leg_vols)
    };

    // When a forecast-target exit is configured, score the trade the way it is
    // actually managed: we take profit at the target, so the payoff is capped at
    // the target's value for any outcome that reaches it, and read at the horizon
    // otherwise. This makes far-OTM lottery tickets — whose target value is ~0 —
    // score poorly, and pulls selection toward strikes inside the forecast.
    let expected_value = match forecast_target_quantile {
        None => dist.expected(value_at),
        Some(q) => {
            let bullish = candidate.spread.profits_from_a_rise();
            let target = dist.quantile(if bullish { q } else { 1.0 - q });
            let value_at_target = value_at(target);
            dist.expected(|s| {
                let reached = if bullish { s >= target } else { s <= target };
                if reached { value_at_target } else { value_at(s) }
            })
        }
    };
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
    prob_profit_of(
        &candidate.spread,
        candidate.cost_basis + candidate.exit_spread_cost,
        &candidate.leg_vols,
        dist,
        remaining_years,
        forward_scale,
    )
}

/// Model probability a spread is worth more than `breakeven` at the horizon —
/// the general form behind a candidate's `prob_profit`, reused by the live-POP
/// exit to re-score an open position against the *current* distribution.
pub fn prob_profit_of(
    spread: &crate::spread::Spread,
    breakeven: f64,
    leg_vols: &[f64],
    dist: &PriceDistribution,
    remaining_years: f64,
    forward_scale: f64,
) -> f64 {
    dist.expected(|s| {
        let value = spread.model_value(s * forward_scale, remaining_years, leg_vols);
        f64::from(value > breakeven)
    })
}

/// The value the ranker actually **orders** by under `metric`, paired with the
/// expected P/L. `RankedChoice::score` stays the expected-P/L ratio because the
/// portfolio model sizes against it; the ordering key is a different question,
/// and it is the one an evaluation of the *ranker* has to grade — grading the
/// expected-value ratio in a `prob_profit` run would report a scorer that run
/// never used.
pub fn ranking_key(
    candidate: &Candidate,
    dist: &PriceDistribution,
    remaining_years: f64,
    forward_scale: f64,
    cfg: &RankingConfig,
) -> (f64, f64) {
    let (expected_pl, score) = expected_pl_and_score(
        candidate,
        dist,
        remaining_years,
        forward_scale,
        cfg.forecast_target_quantile,
    );
    let key = match cfg.metric {
        RankMetric::ExpectedValue => score,
        RankMetric::ProbProfit => prob_profit(candidate, dist, remaining_years, forward_scale),
    };
    (expected_pl, key)
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
        let (expected_pl, score) = expected_pl_and_score(
            &candidate,
            dist,
            remaining_years,
            forward_scale,
            cfg.forecast_target_quantile,
        );
        if expected_pl <= cfg.min_expected_pl {
            continue;
        }
        funnel.positive_ev += 1;

        if score < cfg.min_score {
            continue;
        }
        funnel.above_min_score += 1;

        let prob_profit = prob_profit(&candidate, dist, remaining_years, forward_scale);
        if cfg.min_prob_profit.is_some_and(|floor| prob_profit < floor) {
            continue;
        }
        funnel.above_min_pop += 1;

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

    /// The scorer evaluation grades `ranking_key`, so it has to follow the
    /// configured metric. Before it did, a `prob_profit` run's scorer record was
    /// the expected-value ratio — a validation record about a scorer that run
    /// never used, and identical to the EV ranker's down to the last digit.
    #[test]
    fn the_ordering_key_follows_the_configured_metric() {
        let candidate = bull_call();
        let dist = bullish_distribution();

        let ev = RankingConfig::default();
        let (expected_pl, ev_key) = ranking_key(&candidate, &dist, 0.0, 1.0, &ev);
        assert!((expected_pl - 207.4).abs() < 1e-9);
        assert!((ev_key - 207.4 / 192.6).abs() < 1e-9, "EV orders by E[P/L]/risk");

        let pop = RankingConfig {
            metric: RankMetric::ProbProfit,
            ..RankingConfig::default()
        };
        let (also_expected_pl, pop_key) = ranking_key(&candidate, &dist, 0.0, 1.0, &pop);
        assert!((also_expected_pl - 207.4).abs() < 1e-9, "expected P/L is the metric-free half");
        assert!((pop_key - 0.8).abs() < 1e-9, "prob_profit orders by the hit rate");
    }

    /// The entry gate a short-premium book wants: not "take the best odds
    /// available" but "do not open at all below these odds". Independent of
    /// `min_score`, which floors expected value per unit of risk.
    #[test]
    fn a_probability_floor_refuses_a_candidate_the_score_would_have_taken() {
        // The bull call clears the EV floor comfortably at a 0.8 probability.
        let dist = bullish_distribution();
        let mut funnel = RankingFunnel::default();
        let taken = rank(
            vec![bull_call()],
            &dist,
            0.0,
            1.0,
            &RankingConfig {
                min_prob_profit: Some(0.8),
                ..RankingConfig::default()
            },
            &mut funnel,
        );
        assert!(taken.is_some(), "0.8 clears a floor of 0.8");
        assert_eq!(funnel.above_min_pop, 1);

        // Nudge the floor above it and the same candidate is refused, even
        // though its expected P/L and score are unchanged.
        let mut funnel = RankingFunnel::default();
        let refused = rank(
            vec![bull_call()],
            &dist,
            0.0,
            1.0,
            &RankingConfig {
                min_prob_profit: Some(0.85),
                ..RankingConfig::default()
            },
            &mut funnel,
        );
        assert!(refused.is_none(), "0.8 does not clear a floor of 0.85");
        assert_eq!(funnel.above_min_score, 1, "it cleared the edge floor");
        assert_eq!(funnel.above_min_pop, 0, "and fell out at the odds floor");
        assert_eq!(funnel.chosen, 0);
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
