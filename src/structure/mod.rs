//! Stage two: **what** to build.
//!
//! A structure turns a [`Trigger`] into an abstract [`StructureSpec`] — "long
//! the 0.50 delta call, short the 0.30" — with no strikes in it. Resolving that
//! against a real ladder is stage three's job, and keeping the two apart is what
//! lets the same structure work on a symbol with $1 strikes and one with $5.

use serde::Serialize;

use crate::domain::OptionRight;
use crate::signal::{Direction, MarketView, Trigger};

pub mod vertical;

pub use vertical::Vertical;

/// How to identify one leg on the ladder.
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "by")]
pub enum LegTarget {
    /// **Absolute** delta. Puts are matched on |delta|, so 0.30 names the same
    /// distance out of the money whichever right is being traded.
    Delta { target: f64 },
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum StructureSpec {
    Vertical {
        right: OptionRight,
        long: LegTarget,
        short: LegTarget,
        quantity: u32,
    },
}

pub trait Structure {
    fn name(&self) -> &str;

    /// `None` means "not for this trigger" — a legitimate, silent decline, not a
    /// failure. The engine counts these separately from selector rejections,
    /// because they mean different things.
    fn build(&self, trigger: &Trigger, view: &MarketView<'_>) -> Option<StructureSpec>;
}

/// Whether a right's natural bias agrees with the trigger.
///
/// Calls are naturally bullish and puts naturally bearish, so agreement is what
/// makes a **debit** vertical the directional trade. Disagreement would build the
/// credit version of the same structure, which this crate does not open yet.
pub fn agrees(right: OptionRight, direction: Direction) -> bool {
    matches!(
        (right, direction),
        (OptionRight::Call, Direction::Bullish) | (OptionRight::Put, Direction::Bearish)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calls_agree_with_bullish_and_puts_with_bearish() {
        assert!(agrees(OptionRight::Call, Direction::Bullish));
        assert!(agrees(OptionRight::Put, Direction::Bearish));
        assert!(!agrees(OptionRight::Call, Direction::Bearish));
        assert!(!agrees(OptionRight::Put, Direction::Bullish));
    }
}
