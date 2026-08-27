//! Debit verticals: the bull call spread and the bear put spread.
//!
//! Both are the same shape once strikes are named by **absolute delta**, which
//! is why one struct covers them. For calls, strike rises as delta falls, so the
//! 0.50 delta call sits below the 0.30 and long-0.50/short-0.30 is a bull call.
//! For puts, strike rises as |delta| rises, so the 0.50 put sits *above* the
//! 0.30 and the identical long-0.50/short-0.30 is a bear put. The delta targets
//! do not change; only the right does.
//!
//! Only the debit version is built. A trigger disagreeing with the right's
//! natural bias would make the credit version of the same structure — supported
//! by the payoff maths, but not opened here, so such a trigger is declined.

use super::{LegTarget, Structure, StructureSpec, agrees};
use crate::domain::OptionRight;
use crate::signal::{MarketView, Trigger};
use anyhow::{Result, bail};

#[derive(Debug, Clone)]
pub struct Vertical {
    name: String,
    right: OptionRight,
    long_delta: f64,
    short_delta: f64,
    quantity: u32,
}

impl Vertical {
    pub fn new(
        right: OptionRight,
        long_delta: f64,
        short_delta: f64,
        quantity: u32,
    ) -> Result<Self> {
        for (label, delta) in [("long", long_delta), ("short", short_delta)] {
            if !(0.0..=1.0).contains(&delta) {
                bail!("{label}_delta must be an absolute delta in [0, 1], got {delta}");
            }
        }
        if short_delta >= long_delta {
            // Equal targets would name one strike twice; inverted ones would
            // build the spread backwards without ever looking wrong.
            bail!(
                "short_delta ({short_delta}) must be below long_delta ({long_delta}); \
                 the short leg is the farther-out one"
            );
        }
        if quantity == 0 {
            bail!("quantity must be at least 1");
        }
        Ok(Self {
            name: format!("vertical({right}, {long_delta}/{short_delta} x{quantity})"),
            right,
            long_delta,
            short_delta,
            quantity,
        })
    }

    pub fn bull_call(long_delta: f64, short_delta: f64, quantity: u32) -> Result<Self> {
        Self::new(OptionRight::Call, long_delta, short_delta, quantity)
    }

    pub fn bear_put(long_delta: f64, short_delta: f64, quantity: u32) -> Result<Self> {
        Self::new(OptionRight::Put, long_delta, short_delta, quantity)
    }
}

impl Structure for Vertical {
    fn name(&self) -> &str {
        &self.name
    }

    fn build(&self, trigger: &Trigger, _view: &MarketView<'_>) -> Option<StructureSpec> {
        agrees(self.right, trigger.direction).then_some(StructureSpec::Vertical {
            right: self.right,
            long: LegTarget::Delta {
                target: self.long_delta,
            },
            short: LegTarget::Delta {
                target: self.short_delta,
            },
            quantity: self.quantity,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signal::Direction;
    use crate::signal::testing::bar;

    fn trigger(direction: Direction) -> Trigger {
        Trigger {
            ts: bar(0, 100.0).ts,
            direction,
            strength: 5.0,
        }
    }

    fn build(structure: &Vertical, direction: Direction) -> Option<StructureSpec> {
        let chain = bar(0, 100.0);
        structure.build(
            &trigger(direction),
            &MarketView {
                symbol: "TEST",
                chain: &chain,
            },
        )
    }

    #[test]
    fn a_bullish_trigger_builds_a_bull_call() {
        let v = Vertical::bull_call(0.50, 0.30, 1).unwrap();
        let spec = build(&v, Direction::Bullish).unwrap();
        assert_eq!(
            spec,
            StructureSpec::Vertical {
                right: OptionRight::Call,
                long: LegTarget::Delta { target: 0.50 },
                short: LegTarget::Delta { target: 0.30 },
                quantity: 1,
            }
        );
    }

    #[test]
    fn a_bearish_trigger_builds_a_bear_put() {
        let v = Vertical::bear_put(0.50, 0.30, 2).unwrap();
        let spec = build(&v, Direction::Bearish).unwrap();
        assert_eq!(
            spec,
            StructureSpec::Vertical {
                right: OptionRight::Put,
                long: LegTarget::Delta { target: 0.50 },
                short: LegTarget::Delta { target: 0.30 },
                quantity: 2,
            }
        );
    }

    /// The identical delta targets serve both rights — that is the point of
    /// naming legs by absolute delta rather than by strike offset.
    #[test]
    fn both_rights_take_the_same_delta_targets() {
        let call = build(
            &Vertical::bull_call(0.50, 0.30, 1).unwrap(),
            Direction::Bullish,
        )
        .unwrap();
        let put = build(
            &Vertical::bear_put(0.50, 0.30, 1).unwrap(),
            Direction::Bearish,
        )
        .unwrap();
        let (
            StructureSpec::Vertical {
                long: cl,
                short: cs,
                ..
            },
            StructureSpec::Vertical {
                long: pl,
                short: ps,
                ..
            },
        ) = (&call, &put);
        assert_eq!((cl, cs), (pl, ps));
    }

    #[test]
    fn a_trigger_against_the_rights_bias_is_declined() {
        assert!(
            build(
                &Vertical::bull_call(0.50, 0.30, 1).unwrap(),
                Direction::Bearish
            )
            .is_none()
        );
        assert!(
            build(
                &Vertical::bear_put(0.50, 0.30, 1).unwrap(),
                Direction::Bullish
            )
            .is_none()
        );
    }

    /// An inverted pair would build the spread backwards and still look
    /// plausible in a trade log, so it is refused at construction.
    #[test]
    fn inverted_or_equal_delta_targets_are_rejected() {
        assert!(Vertical::bull_call(0.30, 0.50, 1).is_err());
        assert!(Vertical::bull_call(0.50, 0.50, 1).is_err());
    }

    #[test]
    fn out_of_range_deltas_and_zero_quantity_are_rejected() {
        assert!(Vertical::bull_call(1.5, 0.30, 1).is_err());
        assert!(Vertical::bull_call(0.50, -0.1, 1).is_err());
        assert!(Vertical::bull_call(0.50, 0.30, 0).is_err());
    }
}
