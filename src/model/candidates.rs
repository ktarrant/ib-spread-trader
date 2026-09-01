//! Enumerating the verticals a real ladder can actually supply.
//!
//! This replaces the `structure -> selector` pair for the ranked path. Rather
//! than resolving one delta-targeted structure, it walks every strike pair the
//! chain quotes, in both orientations, and keeps the ones that price cleanly,
//! quote tightly enough to trade, and stay inside the loss cap. Debit and credit
//! verticals both fall out of the same double loop — orientation is read from the
//! sign of the entry debit, not special-cased per right.
//!
//! Every candidate that survives carries what the ranker needs: its entry debit,
//! its structural max profit/loss (net of a round-trip commission), and each
//! leg's entry implied vol, so the ranker can mark it to model at a future spot.

use crate::data::ChainSnapshot;
use crate::domain::OptionRight;
use crate::execution::{Costs, entry_debit};
use crate::spread::{CONTRACT_MULTIPLIER, Leg, Side, Spread};

/// Which verticals to enumerate, and the liquidity and loss limits a candidate
/// must clear. Mapped from TOML in a later phase.
#[derive(Debug, Clone)]
pub struct CandidateConfig {
    pub rights: Vec<OptionRight>,
    pub allow_debit: bool,
    pub allow_credit: bool,
    /// Also enumerate single-leg long calls/puts (no short strike). A naked long
    /// keeps the full upside a vertical caps — efficient far from the money, where
    /// the short leg's credit is a rounding error against the upside it forfeits.
    pub allow_naked_long: bool,
    /// Strike-distance bounds, in the underlying's units.
    pub min_width: f64,
    pub max_width: f64,
    /// A leg's relative bid/ask spread `(ask - bid) / mid` may not exceed this.
    pub max_relative_spread: f64,
    /// Dollar cap on a candidate's max loss (capital at risk).
    pub max_loss: f64,
    pub quantity: u32,
}

impl Default for CandidateConfig {
    fn default() -> Self {
        Self {
            rights: vec![OptionRight::Call, OptionRight::Put],
            allow_debit: true,
            allow_credit: true,
            allow_naked_long: true,
            min_width: 0.0,
            max_width: f64::INFINITY,
            max_relative_spread: 0.5,
            max_loss: 500.0,
            quantity: 1,
        }
    }
}

/// A priceable, liquid, within-loss-cap vertical, ready to be ranked.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub spread: Spread,
    /// Positive for a debit, negative for a credit.
    pub entry_debit: f64,
    /// Total out-of-pocket to enter and exit: `entry_debit + round-trip commission`.
    /// The ranker subtracts this from expected value.
    pub cost_basis: f64,
    /// Structural best outcome net of `cost_basis`; `None` for an uncapped
    /// naked long call. `max_loss` is positive (a long can lose only its premium).
    pub max_profit: Option<f64>,
    pub max_loss: f64,
    /// Entry implied vol per leg, aligned with `spread.legs`, for mark-to-model.
    pub leg_vols: Vec<f64>,
    /// Worst per-leg relative bid/ask spread — the liquidity the candidate cleared.
    pub rel_spread: f64,
    /// Estimated cost of crossing the book to *close*, in dollars: half of each
    /// leg's entry bid/ask spread. The entry crossing is already in `entry_debit`,
    /// but a mark-to-model exit is at mid — so the ranker subtracts this to stop
    /// the score over-crediting a spread it can only exit by crossing.
    pub exit_spread_cost: f64,
}

/// The decision funnel for one bar (or accumulated across a run): how many
/// candidates fell out at each successive gate. Each level is a subset of the one
/// above it, so a sparse result can be told apart from a broken one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct RankingFunnel {
    /// Strike pairs (in an allowed orientation) that priced to a non-zero debit.
    pub enumerated: usize,
    /// …of which had bounded max profit and loss.
    pub priceable: usize,
    /// …of which quoted inside the relative-spread limit.
    pub liquid: usize,
    /// …of which stayed within the loss cap.
    pub within_loss: usize,
    /// …of which had positive expected P/L under the model.
    pub positive_ev: usize,
    /// …of which cleared the minimum score.
    pub above_min_score: usize,
    /// …of which cleared the minimum probability of profit. Equal to
    /// `above_min_score` unless `min_prob_profit` is configured.
    pub above_min_pop: usize,
    /// Bars on which a candidate was actually taken.
    pub chosen: usize,
}

impl RankingFunnel {
    pub fn merge(&mut self, other: &RankingFunnel) {
        self.enumerated += other.enumerated;
        self.priceable += other.priceable;
        self.liquid += other.liquid;
        self.within_loss += other.within_loss;
        self.positive_ev += other.positive_ev;
        self.above_min_score += other.above_min_score;
        self.above_min_pop += other.above_min_pop;
        self.chosen += other.chosen;
    }
}

/// A leg's relative bid/ask spread, `(ask - bid) / mid`. Infinite when the mid is
/// non-positive, so a degenerate quote fails any finite liquidity gate.
fn relative_spread(bid: f64, ask: f64) -> f64 {
    let mid = 0.5 * (bid + ask);
    if mid > 0.0 {
        (ask - bid) / mid
    } else {
        f64::INFINITY
    }
}

/// Enumerate the verticals `chain` can supply under `cfg`, recording the funnel.
pub fn enumerate_verticals(
    chain: &ChainSnapshot,
    costs: &Costs,
    cfg: &CandidateConfig,
    funnel: &mut RankingFunnel,
) -> Vec<Candidate> {
    let mut candidates = Vec::new();

    for &right in &cfg.rights {
        let strikes: Vec<_> = {
            let mut v: Vec<_> = chain.selectable(right).collect();
            v.sort_by(|a, b| a.strike.total_cmp(&b.strike));
            v
        };

        for (a, long) in strikes.iter().enumerate() {
            for (b, short) in strikes.iter().enumerate() {
                if a == b {
                    continue;
                }
                let width = (long.strike - short.strike).abs();
                if width < cfg.min_width || width > cfg.max_width {
                    continue;
                }

                let spread = Spread::new(
                    chain.symbol.clone(),
                    vec![
                        Leg {
                            contract_id: long.contract_id,
                            strike: long.strike,
                            right,
                            side: Side::Long,
                            quantity: cfg.quantity,
                        },
                        Leg {
                            contract_id: short.contract_id,
                            strike: short.strike,
                            right,
                            side: Side::Short,
                            quantity: cfg.quantity,
                        },
                    ],
                );

                let Ok(debit) = entry_debit(&spread, chain) else {
                    continue;
                };
                // A zero debit/credit is stale or crossed data, not a free trade.
                if debit.abs() < 1e-9 {
                    continue;
                }
                let is_credit = debit < 0.0;
                if (is_credit && !cfg.allow_credit) || (!is_credit && !cfg.allow_debit) {
                    continue;
                }
                funnel.enumerated += 1;

                let Some((lo, hi)) = spread.payoff_bounds() else {
                    continue; // unbounded — cannot risk-rank it
                };
                funnel.priceable += 1;

                let rel_spread = relative_spread(long.bid, long.ask)
                    .max(relative_spread(short.bid, short.ask));
                if rel_spread > cfg.max_relative_spread {
                    continue;
                }
                funnel.liquid += 1;

                let round_trip_commission = 2.0 * costs.commission(&spread);
                let cost_basis = debit + round_trip_commission;
                let max_profit = Some(hi - cost_basis);
                let max_loss = cost_basis - lo;
                // A non-positive max loss is a mispriced arbitrage, i.e. bad data;
                // it also makes P/L-per-risk meaningless. Refuse it.
                if max_loss <= 0.0 || max_loss > cfg.max_loss {
                    continue;
                }
                funnel.within_loss += 1;

                let leg_vols = vec![
                    long.implied_vol.expect("selectable leg has a vol"),
                    short.implied_vol.expect("selectable leg has a vol"),
                ];

                // Half of each leg's spread is what closing at mid would give back
                // that a real crossed exit would not.
                let exit_spread_cost = 0.5
                    * ((long.ask - long.bid) + (short.ask - short.bid))
                    * cfg.quantity as f64
                    * CONTRACT_MULTIPLIER;

                candidates.push(Candidate {
                    spread,
                    entry_debit: debit,
                    cost_basis,
                    max_profit,
                    max_loss,
                    leg_vols,
                    rel_spread,
                    exit_spread_cost,
                });
            }
        }

        // Naked long: a single long leg, no short. Its risk is the premium (a
        // long can only lose what it paid), so it clears the same funnel with
        // `max_loss = cost_basis`; a long call's profit is uncapped (`max_profit
        // = None`), a long put's is bounded by a zero underlying.
        if cfg.allow_naked_long {
            for long in &strikes {
                let spread = Spread::new(
                    chain.symbol.clone(),
                    vec![Leg {
                        contract_id: long.contract_id,
                        strike: long.strike,
                        right,
                        side: Side::Long,
                        quantity: cfg.quantity,
                    }],
                );
                let Ok(debit) = entry_debit(&spread, chain) else {
                    continue;
                };
                if debit <= 0.0 {
                    continue; // a long always costs; a non-positive debit is bad data
                }
                funnel.enumerated += 1;
                funnel.priceable += 1; // a long's risk is always bounded

                let rel_spread = relative_spread(long.bid, long.ask);
                if rel_spread > cfg.max_relative_spread {
                    continue;
                }
                funnel.liquid += 1;

                let round_trip_commission = 2.0 * costs.commission(&spread);
                let cost_basis = debit + round_trip_commission;
                let max_loss = cost_basis; // payoff floor is 0: lose only the premium
                if max_loss > cfg.max_loss {
                    continue;
                }
                funnel.within_loss += 1;

                // A long put is capped by a zero underlying; a long call is not.
                let max_profit = match right {
                    OptionRight::Put => {
                        Some(long.strike * cfg.quantity as f64 * CONTRACT_MULTIPLIER - cost_basis)
                    }
                    OptionRight::Call => None,
                };
                let exit_spread_cost =
                    0.5 * (long.ask - long.bid) * cfg.quantity as f64 * CONTRACT_MULTIPLIER;

                candidates.push(Candidate {
                    spread,
                    entry_debit: debit,
                    cost_basis,
                    max_profit,
                    max_loss,
                    leg_vols: vec![long.implied_vol.expect("selectable leg has a vol")],
                    rel_spread,
                    exit_spread_cost,
                });
            }
        }
    }

    candidates
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{OptionQuote, UnderlyingBar};
    use chrono::Utc;

    /// A four-strike call ladder plus a four-strike put ladder around 100.
    fn chain() -> ChainSnapshot {
        let quote = |contract_id, strike: f64, right, bid, ask, delta| OptionQuote {
            contract_id,
            strike,
            right,
            bid,
            ask,
            mid: 0.5 * (bid + ask),
            implied_vol: Some(0.2),
            delta: Some(delta),
        };
        ChainSnapshot {
            symbol: "TEST".into(),
            ts: Utc::now(),
            underlying: UnderlyingBar {
                mid: Some(100.0),
                close: Some(100.0),
                ..UnderlyingBar::default()
            },
            forward: 100.0,
            residual_mad: 0.0,
            paired_strikes: 4,
            year_fraction: 0.05,
            quotes: vec![
                quote(1, 95.0, OptionRight::Call, 6.0, 6.2, 0.75),
                quote(2, 100.0, OptionRight::Call, 2.5, 2.7, 0.50),
                quote(3, 105.0, OptionRight::Call, 0.8, 1.0, 0.28),
                quote(4, 110.0, OptionRight::Call, 0.2, 0.4, 0.12),
                quote(5, 95.0, OptionRight::Put, 0.7, 0.9, -0.25),
                quote(6, 100.0, OptionRight::Put, 2.4, 2.6, -0.50),
                quote(7, 105.0, OptionRight::Put, 5.6, 5.8, -0.74),
            ],
        }
    }

    #[test]
    fn both_debit_and_credit_verticals_are_enumerated() {
        let mut funnel = RankingFunnel::default();
        let cfg = CandidateConfig {
            max_loss: f64::INFINITY,
            ..Default::default()
        };
        let candidates = enumerate_verticals(&chain(), &Costs::default(), &cfg, &mut funnel);

        assert!(candidates.iter().any(|c| c.entry_debit > 0.0), "a debit");
        assert!(candidates.iter().any(|c| c.entry_debit < 0.0), "a credit");
        assert!(candidates.iter().all(|c| c.max_loss > 0.0));
        assert_eq!(funnel.enumerated, funnel.priceable, "verticals are bounded");
    }

    #[test]
    fn naked_longs_are_enumerated_with_premium_as_risk() {
        let mut funnel = RankingFunnel::default();
        let cfg = CandidateConfig {
            rights: vec![OptionRight::Call],
            allow_debit: false,
            allow_credit: false,
            allow_naked_long: true,
            max_loss: f64::INFINITY,
            ..Default::default()
        };
        let candidates = enumerate_verticals(&chain(), &Costs::default(), &cfg, &mut funnel);

        assert!(!candidates.is_empty(), "naked long calls enumerate on their own");
        for c in &candidates {
            assert_eq!(c.spread.legs.len(), 1, "a naked long is a single leg");
            // Risk is the premium; a long call's upside is uncapped.
            assert!((c.max_loss - c.cost_basis).abs() < 1e-9);
            assert_eq!(c.max_profit, None, "a long call has no capped max profit");
        }
    }

    #[test]
    fn a_debit_only_config_drops_the_credits() {
        let mut funnel = RankingFunnel::default();
        let cfg = CandidateConfig {
            allow_credit: false,
            max_loss: f64::INFINITY,
            ..Default::default()
        };
        let candidates = enumerate_verticals(&chain(), &Costs::default(), &cfg, &mut funnel);
        assert!(!candidates.is_empty());
        assert!(candidates.iter().all(|c| c.entry_debit > 0.0));
    }

    #[test]
    fn the_loss_cap_removes_wide_debit_spreads() {
        let mut funnel = RankingFunnel::default();
        // A 95/110 call debit spread risks its whole debit; a tight cap drops the
        // widest structures.
        let cfg = CandidateConfig {
            rights: vec![OptionRight::Call],
            allow_credit: false,
            max_loss: 100.0,
            ..Default::default()
        };
        let candidates = enumerate_verticals(&chain(), &Costs::default(), &cfg, &mut funnel);
        assert!(candidates.iter().all(|c| c.max_loss <= 100.0));
        assert!(funnel.within_loss < funnel.liquid, "some were too wide");
    }

    #[test]
    fn the_relative_spread_gate_rejects_a_wide_book() {
        let mut funnel = RankingFunnel::default();
        let cfg = CandidateConfig {
            max_relative_spread: 0.01, // 1% — tighter than any leg here
            max_loss: f64::INFINITY,
            ..Default::default()
        };
        let candidates = enumerate_verticals(&chain(), &Costs::default(), &cfg, &mut funnel);
        assert!(candidates.is_empty());
        assert_eq!(funnel.liquid, 0);
        assert!(funnel.enumerated > 0, "they priced, they just weren't liquid");
    }
}
