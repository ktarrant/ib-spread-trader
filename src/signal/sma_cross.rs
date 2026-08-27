//! A fast moving average crossing a slow one.

use super::{MarketView, Signal, Trigger, fire};
use crate::indicators::{CrossDetector, Sma};
use anyhow::{Result, bail};

#[derive(Debug)]
pub struct SmaCross {
    name: String,
    fast: Sma,
    slow: Sma,
    detector: CrossDetector,
}

impl SmaCross {
    pub fn new(fast: usize, slow: usize) -> Result<Self> {
        if fast >= slow {
            bail!("fast SMA period ({fast}) must be shorter than the slow one ({slow})");
        }
        Ok(Self {
            name: format!("sma_cross({fast}, {slow})"),
            fast: Sma::new(fast)?,
            slow: Sma::new(slow)?,
            detector: CrossDetector::new(),
        })
    }
}

impl Signal for SmaCross {
    fn name(&self) -> &str {
        &self.name
    }

    fn evaluate(&mut self, view: &MarketView<'_>) -> Option<Trigger> {
        let price = view.price();
        // Both averages are advanced on every bar regardless of whether either
        // is warm; `?` after both would short-circuit and starve the slow one.
        let fast = self.fast.next(price);
        let slow = self.slow.next(price);
        let (fast, slow) = (fast?, slow?);
        let cross = self.detector.update(fast - slow)?;
        Some(fire(view.chain.ts, cross, (fast - slow).abs()))
    }
}

#[cfg(test)]
mod tests {
    use super::super::Direction;
    use super::super::testing::run;
    use super::*;

    /// Down first, so the fast average is *below* the slow when both come warm
    /// and the baseline is set on the correct side. Then up, then down again.
    fn down_up_down() -> Vec<f64> {
        let mut prices: Vec<f64> = (0..25).map(|i| 100.0 - i as f64).collect();
        prices.extend((0..30).map(|i| 76.0 + i as f64 * 2.0));
        prices.extend((0..30).map(|i| 134.0 - i as f64 * 2.0));
        prices
    }

    #[test]
    fn a_reversal_crosses_the_averages_once_in_each_direction() {
        let mut signal = SmaCross::new(5, 20).unwrap();
        let fired = run(&mut signal, &down_up_down());

        assert_eq!(fired.len(), 2, "up on the way out, down on the way back");
        assert_eq!(fired[0].direction, Direction::Bullish);
        assert_eq!(fired[1].direction, Direction::Bearish);
        assert!(fired[0].ts < fired[1].ts);
        assert!(fired.iter().all(|t| t.strength > 0.0));
    }

    /// A property that looks like a bug the first time it is seen, so it is
    /// pinned here: a signal starting **mid-trend** misses the crossing that
    /// trend began with. The averages only become comparable once the slow one
    /// is warm, and whichever side they are on then is a baseline, not an entry.
    #[test]
    fn a_signal_that_starts_mid_trend_misses_that_trends_entry() {
        let mut prices: Vec<f64> = (0..30).map(|i| 100.0 + i as f64).collect();
        prices.extend((0..30).map(|i| 130.0 - i as f64 * 2.0));

        let mut signal = SmaCross::new(5, 20).unwrap();
        let fired = run(&mut signal, &prices);
        assert_eq!(
            fired.len(),
            1,
            "only the reversal, not the rally it started in"
        );
        assert_eq!(fired[0].direction, Direction::Bearish);
    }

    #[test]
    fn a_flat_tape_fires_nothing() {
        let mut signal = SmaCross::new(5, 20).unwrap();
        assert!(run(&mut signal, &[100.0; 60]).is_empty());
    }

    /// Advancing both averages unconditionally is what makes this work: an early
    /// `?` on the fast one would stop feeding the slow one, which would then
    /// never warm and the signal would fire nothing, ever.
    #[test]
    fn nothing_fires_before_the_slow_average_is_warm() {
        let mut signal = SmaCross::new(5, 20).unwrap();
        let fired = run(&mut signal, &down_up_down());
        assert!(!fired.is_empty(), "the slow average did warm");

        let first_possible = crate::signal::testing::bar(19, 0.0).ts;
        assert!(
            fired.iter().all(|t| t.ts >= first_possible),
            "a crossing was reported before 20 bars had been seen"
        );
    }

    #[test]
    fn an_inverted_period_pair_is_rejected() {
        assert!(SmaCross::new(20, 5).is_err());
        assert!(SmaCross::new(5, 5).is_err());
    }
}
