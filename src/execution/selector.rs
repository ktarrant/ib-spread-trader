//! Stage three: **which strikes**, and at what cost.
//!
//! Resolving a [`StructureSpec`] against a real ladder is where a backtest most
//! easily starts lying to itself, because the honest answer is often "you cannot
//! open this trade". On the reference snapshot at 2026-07-15 15:00 UTC, SPY
//! quotes 45 strikes spanning −4.9% to +3.4% of spot, but **META quotes only to
//! +1.0%** — there is no 0.30-delta call on that ladder at all. PLTR and QQQ
//! carry ten strikes each.
//!
//! A selector that quietly took the nearest available strike would turn a trade
//! that could not be placed into a trade that was, at a delta nobody asked for.
//! So every refusal is a typed [`Reject`] carrying **what was available**, and
//! the run report prints a census of them beside the trade log. A strategy whose
//! rejections outnumber its fills has not been backtested; it has been imagined.

use serde::Serialize;

use crate::data::ChainSnapshot;
use crate::domain::OptionRight;
use crate::spread::{Leg, Side, Spread};
use crate::structure::{LegTarget, StructureSpec};

use super::fill::{FillError, entry_debit};

/// Why a spread could not be opened against this bar.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "reject")]
pub enum Reject {
    /// The ladder has no strike within tolerance of the target delta. `best`
    /// carries the closest that *was* quoted, which is what makes a census of
    /// these diagnostic — "off by 0.02" and "off by 0.28" mean different things.
    NoStrikeNearTargetDelta {
        right: OptionRight,
        target: f64,
        best: Option<f64>,
    },
    /// No strike of this right solved for a delta at all.
    NoSelectableStrikes {
        right: OptionRight,
    },
    /// Both targets resolved to the same strike, which is not a spread.
    LegsCollapsedToOneStrike {
        strike: f64,
    },
    /// A debit structure priced as a credit. Real arbitrage does not sit in a
    /// 30-minute bar; this is stale or crossed quotes.
    DebitNotPositive {
        debit: f64,
    },
    DebitAboveLimit {
        debit: f64,
        limit: f64,
    },
    NotPriceable(FillError),
}

impl std::fmt::Display for Reject {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoStrikeNearTargetDelta {
                right,
                target,
                best: Some(best),
            } => write!(
                f,
                "no {right} within tolerance of {target:.2} delta (closest quoted: {best:.3})"
            ),
            Self::NoStrikeNearTargetDelta {
                right,
                target,
                best: None,
            } => {
                write!(
                    f,
                    "no {right} near {target:.2} delta and none quoted at all"
                )
            }
            Self::NoSelectableStrikes { right } => write!(f, "no {right} solved for a delta"),
            Self::LegsCollapsedToOneStrike { strike } => {
                write!(f, "both legs resolved to strike {strike}")
            }
            Self::DebitNotPositive { debit } => write!(f, "debit structure priced at {debit:.2}"),
            Self::DebitAboveLimit { debit, limit } => {
                write!(f, "debit {debit:.2} exceeds the {limit:.2} limit")
            }
            Self::NotPriceable(err) => write!(f, "{err}"),
        }
    }
}

impl Reject {
    /// A stable key for the rejection census.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::NoStrikeNearTargetDelta { .. } => "no_strike_near_target_delta",
            Self::NoSelectableStrikes { .. } => "no_selectable_strikes",
            Self::LegsCollapsedToOneStrike { .. } => "legs_collapsed_to_one_strike",
            Self::DebitNotPositive { .. } => "debit_not_positive",
            Self::DebitAboveLimit { .. } => "debit_above_limit",
            Self::NotPriceable(_) => "not_priceable",
        }
    }
}

pub trait Selector {
    fn resolve(&self, spec: &StructureSpec, chain: &ChainSnapshot) -> Result<Spread, Reject>;
}

/// Picks strikes by absolute delta, refusing anything outside tolerance.
#[derive(Debug, Clone, Copy)]
pub struct DeltaSelector {
    /// How far a strike's |delta| may sit from the target. On a $1-strike SPY
    /// ladder almost anything is reachable; on a five-strike wing it is not.
    pub delta_tolerance: f64,
    /// Cap on capital at risk per spread, in dollars. `None` for no cap.
    pub max_debit: Option<f64>,
}

impl Default for DeltaSelector {
    fn default() -> Self {
        Self {
            delta_tolerance: 0.07,
            max_debit: None,
        }
    }
}

impl DeltaSelector {
    /// The quoted strike of `right` whose |delta| is closest to `target`.
    ///
    /// Only strikes that solved for a delta are candidates — one quoting at or
    /// below intrinsic has no delta, and is unusable rather than zero-delta.
    fn nearest(
        &self,
        chain: &ChainSnapshot,
        right: OptionRight,
        target: f64,
    ) -> Result<(f64, i64), Reject> {
        let mut best: Option<(f64, f64, i64)> = None; // (|delta|, strike, contract_id)
        for quote in chain.selectable(right) {
            let magnitude = quote.delta.expect("selectable implies a delta").abs();
            let gap = (magnitude - target).abs();
            if best.is_none_or(|(current, _, _)| gap < (current - target).abs()) {
                best = Some((magnitude, quote.strike, quote.contract_id));
            }
        }

        let Some((magnitude, strike, contract_id)) = best else {
            return Err(Reject::NoSelectableStrikes { right });
        };
        if (magnitude - target).abs() > self.delta_tolerance {
            return Err(Reject::NoStrikeNearTargetDelta {
                right,
                target,
                best: Some(magnitude),
            });
        }
        Ok((strike, contract_id))
    }
}

impl Selector for DeltaSelector {
    fn resolve(&self, spec: &StructureSpec, chain: &ChainSnapshot) -> Result<Spread, Reject> {
        let StructureSpec::Vertical {
            right,
            long,
            short,
            quantity,
        } = spec;
        let LegTarget::Delta {
            target: long_target,
        } = long;
        let LegTarget::Delta {
            target: short_target,
        } = short;

        let (long_strike, long_id) = self.nearest(chain, *right, *long_target)?;
        let (short_strike, short_id) = self.nearest(chain, *right, *short_target)?;

        if long_strike == short_strike {
            return Err(Reject::LegsCollapsedToOneStrike {
                strike: long_strike,
            });
        }

        let spread = Spread::new(
            chain.symbol.clone(),
            vec![
                Leg {
                    contract_id: long_id,
                    strike: long_strike,
                    right: *right,
                    side: Side::Long,
                    quantity: *quantity,
                },
                Leg {
                    contract_id: short_id,
                    strike: short_strike,
                    right: *right,
                    side: Side::Short,
                    quantity: *quantity,
                },
            ],
        );

        let debit = entry_debit(&spread, chain).map_err(Reject::NotPriceable)?;
        if debit <= 0.0 {
            return Err(Reject::DebitNotPositive { debit });
        }
        if let Some(limit) = self.max_debit
            && debit > limit
        {
            return Err(Reject::DebitAboveLimit { debit, limit });
        }
        Ok(spread)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::chain::{OptionQuote, UnderlyingBar};
    use crate::pricing;
    use chrono::{TimeZone, Utc};

    /// A realistic call ladder: strikes around a 100 forward, with real
    /// Black-76 deltas so the selector is exercised against the actual surface
    /// shape rather than made-up numbers.
    fn ladder(strikes: &[f64]) -> ChainSnapshot {
        let (forward, t, vol) = (100.0, 0.08, 0.25);
        let quotes = strikes
            .iter()
            .enumerate()
            .map(|(i, &strike)| {
                let price = pricing::black76(forward, strike, t, vol, true);
                OptionQuote {
                    contract_id: i as i64 + 1,
                    strike,
                    right: OptionRight::Call,
                    bid: price - 0.05,
                    ask: price + 0.05,
                    mid: price,
                    implied_vol: Some(vol),
                    delta: pricing::delta(forward, strike, t, vol, true),
                }
            })
            .collect();
        ChainSnapshot {
            symbol: "TEST".into(),
            expiry: chrono::NaiveDate::from_ymd_opt(2026, 8, 21).unwrap(),
            ts: Utc.with_ymd_and_hms(2026, 7, 15, 15, 0, 0).unwrap(),
            underlying: UnderlyingBar {
                mid: Some(100.0),
                close: Some(100.0),
                ..UnderlyingBar::default()
            },
            forward,
            residual_mad: 0.02,
            paired_strikes: strikes.len(),
            year_fraction: t,
            interpolated: Vec::new(),
            quotes,
        }
    }

    fn bull_call(long: f64, short: f64) -> StructureSpec {
        StructureSpec::Vertical {
            right: OptionRight::Call,
            long: LegTarget::Delta { target: long },
            short: LegTarget::Delta { target: short },
            quantity: 1,
        }
    }

    fn wide() -> ChainSnapshot {
        ladder(&[
            90.0, 92.0, 94.0, 96.0, 98.0, 100.0, 102.0, 104.0, 106.0, 108.0, 110.0,
        ])
    }

    #[test]
    fn a_dense_ladder_resolves_both_legs_at_the_asked_for_deltas() {
        let chain = wide();
        let spread = DeltaSelector::default()
            .resolve(&bull_call(0.50, 0.30), &chain)
            .unwrap();

        assert_eq!(spread.legs.len(), 2);
        assert_eq!(spread.legs[0].side, Side::Long);
        assert_eq!(spread.legs[1].side, Side::Short);
        assert!(
            spread.legs[0].strike < spread.legs[1].strike,
            "a bull call is long the lower strike"
        );

        for (leg, target) in spread.legs.iter().zip([0.50, 0.30]) {
            let delta = chain.quote(leg.contract_id).unwrap().delta.unwrap();
            assert!(
                (delta.abs() - target).abs() <= 0.07,
                "leg at {delta} vs target {target}"
            );
        }
    }

    /// The META case, which is the reason rejection is typed rather than a bool:
    /// the ladder stops just above the money, so there is no 0.30-delta call on
    /// it. The rejection has to say what *was* there.
    #[test]
    fn a_ladder_that_stops_near_the_money_rejects_and_says_what_it_had() {
        let truncated = ladder(&[94.0, 96.0, 98.0, 100.0, 101.0]);
        let err = DeltaSelector::default()
            .resolve(&bull_call(0.50, 0.30), &truncated)
            .unwrap_err();

        let Reject::NoStrikeNearTargetDelta {
            right,
            target,
            best,
        } = err
        else {
            panic!("expected a delta rejection, got {err:?}");
        };
        assert_eq!(right, OptionRight::Call);
        assert_eq!(target, 0.30);
        let best = best.expect("the ladder did quote something");
        assert!(
            best > 0.30 + 0.07,
            "the closest available was {best}, still too deep"
        );
    }

    #[test]
    fn a_wider_tolerance_accepts_what_the_default_refused() {
        let truncated = ladder(&[94.0, 96.0, 98.0, 100.0, 101.0]);
        let spec = bull_call(0.50, 0.30);
        assert!(DeltaSelector::default().resolve(&spec, &truncated).is_err());

        let permissive = DeltaSelector {
            delta_tolerance: 0.40,
            ..Default::default()
        };
        assert!(permissive.resolve(&spec, &truncated).is_ok());
    }

    /// Strikes with no solvable delta are not candidates at all — picking one
    /// would be picking a strike whose distance from the money is unknown.
    #[test]
    fn strikes_without_a_delta_are_never_selected() {
        let mut chain = wide();
        for quote in &mut chain.quotes {
            if quote.strike <= 100.0 {
                quote.delta = None;
                quote.implied_vol = None;
            }
        }
        let spread = DeltaSelector {
            delta_tolerance: 1.0,
            ..Default::default()
        }
        .resolve(&bull_call(0.50, 0.30), &chain)
        .unwrap();
        assert!(spread.legs.iter().all(|leg| leg.strike > 100.0));
    }

    #[test]
    fn a_ladder_with_no_solvable_strikes_rejects() {
        let mut chain = wide();
        for quote in &mut chain.quotes {
            quote.delta = None;
        }
        assert_eq!(
            DeltaSelector::default()
                .resolve(&bull_call(0.50, 0.30), &chain)
                .unwrap_err(),
            Reject::NoSelectableStrikes {
                right: OptionRight::Call
            }
        );
    }

    /// A ladder so sparse that both targets land on one strike is not a spread.
    #[test]
    fn two_targets_landing_on_one_strike_reject() {
        let single = ladder(&[100.0]);
        let permissive = DeltaSelector {
            delta_tolerance: 1.0,
            ..Default::default()
        };
        let err = permissive
            .resolve(&bull_call(0.50, 0.30), &single)
            .unwrap_err();
        assert!(
            matches!(err, Reject::LegsCollapsedToOneStrike { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn a_debit_above_the_limit_rejects_and_reports_both_numbers() {
        let chain = wide();
        let selector = DeltaSelector {
            delta_tolerance: 0.07,
            max_debit: Some(10.0),
        };
        let err = selector
            .resolve(&bull_call(0.50, 0.30), &chain)
            .unwrap_err();
        let Reject::DebitAboveLimit { debit, limit } = err else {
            panic!("expected a debit rejection, got {err:?}");
        };
        assert_eq!(limit, 10.0);
        assert!(debit > limit);
    }

    #[test]
    fn a_generous_limit_lets_the_same_spread_through() {
        let selector = DeltaSelector {
            delta_tolerance: 0.07,
            max_debit: Some(100_000.0),
        };
        assert!(selector.resolve(&bull_call(0.50, 0.30), &wide()).is_ok());
    }

    /// Stale or crossed quotes can make a debit spread price as a credit. That is
    /// not free money, it is bad data, and taking it would show up as edge.
    #[test]
    fn a_debit_spread_pricing_as_a_credit_rejects() {
        let mut chain = wide();
        // Make the far leg richer than the near one.
        for quote in &mut chain.quotes {
            if quote.strike >= 104.0 {
                quote.bid = 50.0;
                quote.ask = 51.0;
            }
        }
        let err = DeltaSelector::default()
            .resolve(&bull_call(0.50, 0.30), &chain)
            .unwrap_err();
        assert!(matches!(err, Reject::DebitNotPositive { .. }), "{err:?}");
    }

    #[test]
    fn every_rejection_has_a_stable_census_key_and_a_readable_message() {
        let cases = [
            Reject::NoStrikeNearTargetDelta {
                right: OptionRight::Call,
                target: 0.3,
                best: Some(0.61),
            },
            Reject::NoStrikeNearTargetDelta {
                right: OptionRight::Put,
                target: 0.3,
                best: None,
            },
            Reject::NoSelectableStrikes {
                right: OptionRight::Call,
            },
            Reject::LegsCollapsedToOneStrike { strike: 100.0 },
            Reject::DebitNotPositive { debit: -5.0 },
            Reject::DebitAboveLimit {
                debit: 900.0,
                limit: 500.0,
            },
        ];
        for case in &cases {
            assert!(!case.kind().is_empty());
            assert!(!case.to_string().is_empty(), "{case:?} has no message");
        }
        // Both delta variants share one census key on purpose: "no strike near
        // the target" is one finding whether or not the ladder quoted anything.
        assert_eq!(cases[0].kind(), cases[1].kind());
        let distinct: std::collections::HashSet<_> = cases.iter().map(|r| r.kind()).collect();
        assert_eq!(distinct.len(), cases.len() - 1);
    }
}
