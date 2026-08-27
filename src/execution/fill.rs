//! What a spread actually costs to open and close.
//!
//! **Conservative executable pricing.** A buyer lifts the offer and a seller
//! hits the bid, in both directions. Opening a long leg pays the ask; opening a
//! short leg receives the bid; closing reverses both. Nothing here ever fills at
//! the midpoint.
//!
//! This is measurable rather than assumed only because the capture stores `bid`
//! and `ask` as *separate* bar series instead of a blended `bid_ask` — a
//! deliberate choice on the capture side, since requesting them separately costs
//! the same pacing budget and returns strictly more information.
//!
//! It matters more than it looks. SPY calls average a 2.45% relative spread, and
//! a vertical crosses it four times over a round trip. A mid-to-mid backtest can
//! manufacture most of a strategy's apparent edge out of nothing.

use serde::Serialize;

use crate::data::ChainSnapshot;
use crate::spread::{CONTRACT_MULTIPLIER, Side, Spread};

/// Per-contract, per-leg trading costs.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Costs {
    /// IB tiered US equity options run about $0.65 a contract. Charged per leg,
    /// never per order: a 1-lot vertical is ~$2.60 round trip, which is material
    /// against a $2-wide spread.
    pub commission_per_contract: f64,
}

impl Default for Costs {
    fn default() -> Self {
        Self {
            commission_per_contract: 0.65,
        }
    }
}

impl Costs {
    pub fn commission(&self, spread: &Spread) -> f64 {
        spread.total_contracts() as f64 * self.commission_per_contract
    }
}

/// Whether a leg is being acquired or disposed of, which is what decides the
/// side of the book it crosses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Open,
    Close,
}

/// Why a spread could not be priced against a bar.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "reason")]
pub enum FillError {
    /// The bar has no quote for one of the legs. Common near the wings, where a
    /// strike simply does not print every 30 minutes.
    LegNotQuoted { contract_id: i64, strike: f64 },
}

impl std::fmt::Display for FillError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LegNotQuoted { strike, .. } => write!(f, "strike {strike} is not quoted"),
        }
    }
}

/// Signed cash flow for `spread` against `bar`, in dollars.
///
/// Negative is cash out. Opening a debit spread returns a negative number;
/// closing it for a credit returns a positive one. Commission is **not**
/// included — the caller charges it separately so it stays visible in reports.
pub fn cash_flow(
    spread: &Spread,
    bar: &ChainSnapshot,
    direction: Direction,
) -> Result<f64, FillError> {
    let mut total = 0.0;
    for leg in &spread.legs {
        let quote = bar.quote(leg.contract_id).ok_or(FillError::LegNotQuoted {
            contract_id: leg.contract_id,
            strike: leg.strike,
        })?;

        // Acquiring a contract crosses to the ask; disposing of one crosses to
        // the bid. Opening a long and closing a short both acquire.
        let acquiring = matches!(
            (leg.side, direction),
            (Side::Long, Direction::Open) | (Side::Short, Direction::Close)
        );
        let price = if acquiring { quote.ask() } else { quote.bid() };
        let sign = if acquiring { -1.0 } else { 1.0 };
        total += sign * price * leg.quantity as f64 * CONTRACT_MULTIPLIER;
    }
    Ok(total)
}

/// What it costs to open, as a positive debit (or negative for a net credit).
pub fn entry_debit(spread: &Spread, bar: &ChainSnapshot) -> Result<f64, FillError> {
    cash_flow(spread, bar, Direction::Open).map(|flow| -flow)
}

/// What closing it right now would return, as a positive credit.
pub fn exit_credit(spread: &Spread, bar: &ChainSnapshot) -> Result<f64, FillError> {
    cash_flow(spread, bar, Direction::Close)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::chain::{OptionQuote, UnderlyingBar};
    use crate::domain::OptionRight;
    use crate::spread::Leg;
    use chrono::{TimeZone, Utc};

    fn quote(contract_id: i64, strike: f64, right: OptionRight, bid: f64, ask: f64) -> OptionQuote {
        OptionQuote {
            contract_id,
            strike,
            right,
            bid,
            ask,
            mid: 0.5 * (bid + ask),
            implied_vol: Some(0.2),
            delta: Some(0.5),
        }
    }

    /// A 750/755 bull call. The long leg is 2.90/3.10 and the short 1.40/1.60,
    /// so mid-to-mid the spread looks like 3.00 - 1.50 = $150.
    fn bar() -> ChainSnapshot {
        ChainSnapshot {
            symbol: "SPY".into(),
            ts: Utc.with_ymd_and_hms(2026, 8, 19, 15, 0, 0).unwrap(),
            underlying: UnderlyingBar {
                mid: Some(751.0),
                close: Some(751.0),
            },
            forward: 751.0,
            residual_mad: 0.02,
            paired_strikes: 30,
            year_fraction: 0.006,
            quotes: vec![
                quote(1, 750.0, OptionRight::Call, 2.90, 3.10),
                quote(2, 755.0, OptionRight::Call, 1.40, 1.60),
            ],
        }
    }

    fn bull_call(quantity: u32) -> Spread {
        Spread::new(
            "SPY",
            vec![
                Leg {
                    contract_id: 1,
                    strike: 750.0,
                    right: OptionRight::Call,
                    side: Side::Long,
                    quantity,
                },
                Leg {
                    contract_id: 2,
                    strike: 755.0,
                    right: OptionRight::Call,
                    side: Side::Short,
                    quantity,
                },
            ],
        )
    }

    /// Pay the ask on the long, receive the bid on the short: 3.10 - 1.40 = 1.70.
    #[test]
    fn opening_crosses_the_spread_on_both_legs() {
        assert_eq!(entry_debit(&bull_call(1), &bar()).unwrap(), 170.0);
    }

    /// Receive the bid on the long, pay the ask on the short: 2.90 - 1.60 = 1.30.
    #[test]
    fn closing_crosses_the_spread_back() {
        assert_eq!(exit_credit(&bull_call(1), &bar()).unwrap(), 130.0);
    }

    /// The whole reason mid-to-mid is not offered. Opening and immediately
    /// closing loses $40 on quotes that never moved — and mid pricing would have
    /// scored it at exactly zero, inventing $40 of edge per round trip.
    #[test]
    fn an_instant_round_trip_loses_the_spread_not_zero() {
        let (spread, bar) = (bull_call(1), bar());
        let round_trip = exit_credit(&spread, &bar).unwrap() - entry_debit(&spread, &bar).unwrap();
        assert_eq!(round_trip, -40.0);

        let mid_to_mid = 0.5 * (2.90 + 3.10) - 0.5 * (1.40 + 1.60);
        assert_eq!(
            mid_to_mid * CONTRACT_MULTIPLIER,
            150.0,
            "what mid pricing would have said"
        );
    }

    #[test]
    fn commission_is_charged_per_leg_and_per_contract() {
        let costs = Costs::default();
        assert!((costs.commission(&bull_call(1)) - 1.30).abs() < 1e-9);
        assert!((costs.commission(&bull_call(3)) - 3.90).abs() < 1e-9);
        // The round trip is what a $2-wide spread has to clear.
        assert!((2.0 * costs.commission(&bull_call(1)) - 2.60).abs() < 1e-9);
    }

    #[test]
    fn quantity_scales_the_cash_flow() {
        assert_eq!(entry_debit(&bull_call(4), &bar()).unwrap(), 680.0);
    }

    /// A credit spread is the same code path with the sign reversed.
    #[test]
    fn a_credit_spread_opens_for_a_negative_debit() {
        let bear_call = Spread::new(
            "SPY",
            vec![
                Leg {
                    contract_id: 1,
                    strike: 750.0,
                    right: OptionRight::Call,
                    side: Side::Short,
                    quantity: 1,
                },
                Leg {
                    contract_id: 2,
                    strike: 755.0,
                    right: OptionRight::Call,
                    side: Side::Long,
                    quantity: 1,
                },
            ],
        );
        // Receive 2.90 on the short, pay 1.60 on the long: a $130 credit.
        assert_eq!(entry_debit(&bear_call, &bar()).unwrap(), -130.0);
    }

    #[test]
    fn an_unquoted_leg_is_an_error_not_a_guess() {
        let spread = Spread::new(
            "SPY",
            vec![Leg {
                contract_id: 99,
                strike: 800.0,
                right: OptionRight::Call,
                side: Side::Long,
                quantity: 1,
            }],
        );
        assert_eq!(
            entry_debit(&spread, &bar()),
            Err(FillError::LegNotQuoted {
                contract_id: 99,
                strike: 800.0
            })
        );
    }
}
