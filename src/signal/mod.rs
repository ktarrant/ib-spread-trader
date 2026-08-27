//! Stage one of the pipeline: **when** to act.
//!
//! A signal sees one bar at a time, in order, and either fires or does not.
//! Everything it needs to carry forward lives in its own state, so no signal can
//! reach backwards into history it was not given — which is what makes
//! look-ahead structurally impossible rather than merely avoided.
//!
//! [`Trigger`] is the only thing crossing into stage two, so any signal composes
//! with any structure.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::data::ChainSnapshot;
use crate::indicators::Cross;

pub mod rsi_cross;
pub mod sma_cross;

pub use rsi_cross::RsiCross;
pub use sma_cross::SmaCross;

/// Which way a signal thinks the underlying is going.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Bullish,
    Bearish,
}

impl From<Cross> for Direction {
    /// A crossing up reads bullish, a crossing down bearish. A strategy wanting
    /// the contrarian reading inverts at the structure stage, not here — keeping
    /// the mapping fixed means a `Trigger` means the same thing whatever produced it.
    fn from(cross: Cross) -> Self {
        match cross {
            Cross::Up => Self::Bullish,
            Cross::Down => Self::Bearish,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct Trigger {
    pub ts: DateTime<Utc>,
    pub direction: Direction,
    /// How emphatic the crossing was, in the signal's own units. Recorded on the
    /// trade log; nothing sizes off it yet.
    pub strength: f64,
}

/// What a signal is allowed to see at one bar.
#[derive(Debug, Clone, Copy)]
pub struct MarketView<'a> {
    pub symbol: &'a str,
    pub chain: &'a ChainSnapshot,
}

impl MarketView<'_> {
    /// The underlying price to compute indicators on: real spot where the
    /// capture has it, the parity forward otherwise.
    pub fn price(&self) -> f64 {
        self.chain.reference_price()
    }
}

pub trait Signal {
    fn name(&self) -> &str;

    /// Called **once per bar, in order**. Stateful indicators depend on that;
    /// skipping a bar or replaying one silently corrupts every reading after it.
    fn evaluate(&mut self, view: &MarketView<'_>) -> Option<Trigger>;
}

/// Shared by the level- and series-crossing signals below.
fn fire(ts: DateTime<Utc>, cross: Cross, strength: f64) -> Trigger {
    Trigger {
        ts,
        direction: cross.into(),
        strength,
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;
    use crate::data::chain::UnderlyingBar;
    use chrono::TimeZone;

    /// A bar carrying nothing but a price — enough for any signal that reads the
    /// underlying, which is all of them so far.
    pub fn bar(minute: u32, price: f64) -> ChainSnapshot {
        ChainSnapshot {
            ts: Utc
                .with_ymd_and_hms(2026, 7, 1, 13 + minute / 60, minute % 60, 0)
                .unwrap(),
            underlying: UnderlyingBar {
                mid: Some(price),
                close: Some(price),
            },
            forward: price,
            residual_mad: 0.01,
            paired_strikes: 30,
            year_fraction: 0.1,
            quotes: Vec::new(),
        }
    }

    /// Drive a signal over a price path and collect what it fired.
    pub fn run(signal: &mut dyn Signal, prices: &[f64]) -> Vec<Trigger> {
        prices
            .iter()
            .enumerate()
            .filter_map(|(i, &price)| {
                let chain = bar(i as u32, price);
                signal.evaluate(&MarketView {
                    symbol: "TEST",
                    chain: &chain,
                })
            })
            .collect()
    }
}
