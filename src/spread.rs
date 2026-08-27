//! A concrete, priceable option structure: legs with resolved contract ids.
//!
//! Everything downstream of the selector speaks in `Spread`. Its payoff is
//! evaluated generically rather than per-structure — the expiry payoff of any
//! set of same-expiry legs is piecewise linear with breaks only at the strikes,
//! so sampling the breakpoints and both tails finds the true extremes. A
//! vertical, a butterfly and a condor all fall out of the same three lines,
//! which is what stops `max_profit` from silently meaning something different
//! the first time a non-vertical structure is added.

use serde::Serialize;

use crate::domain::OptionRight;

/// The capture stores 100 on every option contract, and this crate only trades
/// standard US equity options. Kept named rather than inlined so the one place
/// it would need to change is findable.
pub const CONTRACT_MULTIPLIER: f64 = 100.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Side {
    Long,
    Short,
}

impl Side {
    /// +1 long, -1 short. Quantities are always positive; the side carries sign.
    pub fn sign(self) -> f64 {
        match self {
            Self::Long => 1.0,
            Self::Short => -1.0,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Leg {
    pub contract_id: i64,
    pub strike: f64,
    pub right: OptionRight,
    pub side: Side,
    /// Contracts. Always positive.
    pub quantity: u32,
}

impl Leg {
    /// This leg's intrinsic value at expiry, in dollars, signed by side.
    pub fn payoff_at(&self, underlying: f64) -> f64 {
        let intrinsic = match self.right {
            OptionRight::Call => (underlying - self.strike).max(0.0),
            OptionRight::Put => (self.strike - underlying).max(0.0),
        };
        self.side.sign() * intrinsic * self.quantity as f64 * CONTRACT_MULTIPLIER
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Spread {
    pub symbol: String,
    pub legs: Vec<Leg>,
}

impl Spread {
    pub fn new(symbol: impl Into<String>, legs: Vec<Leg>) -> Self {
        Self {
            symbol: symbol.into(),
            legs,
        }
    }

    /// Total intrinsic value of the structure at expiry, in dollars.
    pub fn payoff_at(&self, underlying: f64) -> f64 {
        self.legs.iter().map(|leg| leg.payoff_at(underlying)).sum()
    }

    /// Underlying prices where the payoff can turn: every strike, plus a point
    /// below the lowest and above the highest to catch unbounded tails.
    fn breakpoints(&self) -> Vec<f64> {
        let mut points: Vec<f64> = self.legs.iter().map(|leg| leg.strike).collect();
        points.sort_by(|a, b| a.partial_cmp(b).expect("strikes are finite"));
        points.dedup();
        let highest = points.last().copied().unwrap_or(0.0);
        points.insert(0, 0.0);
        // Far enough past the last strike that an uncovered leg's tail dominates.
        points.push(highest * 2.0 + 1.0);
        points
    }

    /// Best and worst outcome at expiry, in dollars, **before** entry cost.
    ///
    /// Returns `None` on an unbounded structure — a naked long side whose payoff
    /// grows without limit. A backtester must not report a finite max profit for
    /// one, and a stop expressed as a fraction of max profit is meaningless there.
    pub fn payoff_bounds(&self) -> Option<(f64, f64)> {
        if self.legs.is_empty() {
            return None;
        }
        let points = self.breakpoints();
        let (mut lo, mut hi) = (f64::INFINITY, f64::NEG_INFINITY);
        for &p in &points {
            let value = self.payoff_at(p);
            lo = lo.min(value);
            hi = hi.max(value);
        }

        // Slope past the last strike: if it is non-zero the payoff runs away in
        // that direction, and the sampled extreme is an artefact of where the
        // sample stopped rather than a real bound. Only the upper tail needs
        // this test — the lower one is bounded by the underlying reaching zero,
        // which the 0.0 breakpoint already evaluates.
        let far = points.last().copied().unwrap_or(0.0);
        let slope = (self.payoff_at(far + 1.0) - self.payoff_at(far)).abs();
        if slope > 1e-9 {
            return None;
        }
        Some((lo, hi))
    }

    pub fn total_contracts(&self) -> u32 {
        self.legs.iter().map(|leg| leg.quantity).sum()
    }

    pub fn describe(&self) -> String {
        let mut parts: Vec<String> = self
            .legs
            .iter()
            .map(|leg| {
                let sign = if leg.side == Side::Long { "+" } else { "-" };
                format!(
                    "{sign}{}{} {}",
                    leg.quantity,
                    leg.right.as_storage(),
                    leg.strike
                )
            })
            .collect();
        parts.sort();
        format!("{} {}", self.symbol, parts.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leg(strike: f64, right: OptionRight, side: Side) -> Leg {
        Leg {
            contract_id: strike as i64,
            strike,
            right,
            side,
            quantity: 1,
        }
    }

    fn bull_call() -> Spread {
        Spread::new(
            "SPY",
            vec![
                leg(750.0, OptionRight::Call, Side::Long),
                leg(755.0, OptionRight::Call, Side::Short),
            ],
        )
    }

    #[test]
    fn a_bull_call_pays_zero_below_and_the_width_above() {
        let s = bull_call();
        assert_eq!(s.payoff_at(700.0), 0.0);
        assert_eq!(s.payoff_at(752.5), 250.0);
        assert_eq!(s.payoff_at(755.0), 500.0);
        assert_eq!(s.payoff_at(900.0), 500.0, "capped by the short leg");
    }

    #[test]
    fn a_vertical_is_bounded_by_zero_and_its_width() {
        let (lo, hi) = bull_call().payoff_bounds().unwrap();
        assert_eq!((lo, hi), (0.0, 500.0));
    }

    /// The same three lines have to hold for a structure nobody has written a
    /// special case for.
    #[test]
    fn a_butterfly_peaks_at_its_body() {
        let fly = Spread::new(
            "SPY",
            vec![
                leg(745.0, OptionRight::Call, Side::Long),
                Leg {
                    quantity: 2,
                    ..leg(750.0, OptionRight::Call, Side::Short)
                },
                leg(755.0, OptionRight::Call, Side::Long),
            ],
        );
        assert_eq!(fly.payoff_at(700.0), 0.0);
        assert_eq!(fly.payoff_at(750.0), 500.0);
        assert_eq!(fly.payoff_at(900.0), 0.0);
        assert_eq!(fly.payoff_bounds().unwrap(), (0.0, 500.0));
    }

    #[test]
    fn a_bear_put_is_the_mirror_image() {
        let s = Spread::new(
            "SPY",
            vec![
                leg(755.0, OptionRight::Put, Side::Long),
                leg(750.0, OptionRight::Put, Side::Short),
            ],
        );
        assert_eq!(s.payoff_at(900.0), 0.0);
        assert_eq!(s.payoff_at(700.0), 500.0);
        assert_eq!(s.payoff_bounds().unwrap(), (0.0, 500.0));
    }

    /// Only the upper tail can run away, and both directions of it count: a long
    /// call has no finite maximum, a short call no finite minimum. Reporting a
    /// bound for either would make a profit target expressed as a percentage of
    /// max profit a fiction.
    #[test]
    fn a_structure_with_a_runaway_tail_reports_no_bounds() {
        for side in [Side::Long, Side::Short] {
            let naked = Spread::new("SPY", vec![leg(750.0, OptionRight::Call, side)]);
            assert_eq!(naked.payoff_bounds(), None, "naked {side:?} call");
        }
    }

    /// Puts, by contrast, are always bounded — the underlying cannot go below
    /// zero, so a short put's worst case is simply the strike. Rejecting these
    /// as unbounded would refuse perfectly tradeable structures.
    #[test]
    fn naked_puts_are_bounded_by_a_zero_underlying() {
        let short = Spread::new("SPY", vec![leg(750.0, OptionRight::Put, Side::Short)]);
        assert_eq!(short.payoff_bounds().unwrap(), (-75_000.0, 0.0));

        let long = Spread::new("SPY", vec![leg(750.0, OptionRight::Put, Side::Long)]);
        assert_eq!(long.payoff_bounds().unwrap(), (0.0, 75_000.0));
    }

    #[test]
    fn quantity_scales_the_payoff() {
        let mut s = bull_call();
        for l in &mut s.legs {
            l.quantity = 3;
        }
        assert_eq!(s.payoff_at(900.0), 1500.0);
        assert_eq!(s.total_contracts(), 6);
    }

    #[test]
    fn describe_names_the_structure() {
        assert_eq!(bull_call().describe(), "SPY +1C 750 -1C 755");
    }
}
