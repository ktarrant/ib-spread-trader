//! RSI crossing a fixed level.

use super::{MarketView, Signal, Trigger, fire};
use crate::indicators::{CrossDetector, Rsi};
use anyhow::Result;

#[derive(Debug)]
pub struct RsiCross {
    name: String,
    level: f64,
    rsi: Rsi,
    detector: CrossDetector,
}

impl RsiCross {
    pub fn new(period: usize, level: f64) -> Result<Self> {
        Ok(Self {
            name: format!("rsi_cross({period}, {level})"),
            level,
            rsi: Rsi::new(period)?,
            detector: CrossDetector::new(),
        })
    }
}

impl Signal for RsiCross {
    fn name(&self) -> &str {
        &self.name
    }

    fn evaluate(&mut self, view: &MarketView<'_>) -> Option<Trigger> {
        // The RSI is fed on every bar even while warming up; only the *reading*
        // is withheld. Skipping the feed would just move the warm-up later.
        let reading = self.rsi.next(view.price())?;
        let cross = self.detector.update(reading - self.level)?;
        Some(fire(view.chain.ts, cross, (reading - self.level).abs()))
    }
}

#[cfg(test)]
mod tests {
    use super::super::Direction;
    use super::super::testing::run;
    use super::*;

    /// A V: down hard enough to push RSI under 30, then back up through it.
    fn v_shape() -> Vec<f64> {
        let mut prices = vec![100.0];
        for _ in 0..20 {
            prices.push(prices.last().unwrap() - 1.0);
        }
        for _ in 0..20 {
            prices.push(prices.last().unwrap() + 1.5);
        }
        prices
    }

    #[test]
    fn a_recovery_off_an_oversold_low_fires_bullish() {
        let mut signal = RsiCross::new(14, 30.0).unwrap();
        let fired = run(&mut signal, &v_shape());
        assert_eq!(fired.len(), 1, "one crossing, fired once");
        assert_eq!(fired[0].direction, Direction::Bullish);
        assert!(fired[0].strength > 0.0);
    }

    /// The whole point of the warm-up guard, end to end: a monotone rise has no
    /// crossing in it at all, but an unsuppressed RSI opens at 50 and would read
    /// the first meaningful bar as a cross up through 30.
    #[test]
    fn a_steady_climb_fires_nothing() {
        let prices: Vec<f64> = (0..60).map(|i| 100.0 + i as f64).collect();
        let mut signal = RsiCross::new(14, 30.0).unwrap();
        assert!(run(&mut signal, &prices).is_empty());
    }

    #[test]
    fn a_flat_tape_fires_nothing() {
        let mut signal = RsiCross::new(14, 30.0).unwrap();
        assert!(run(&mut signal, &[100.0; 60]).is_empty());
    }

    /// The same path against a 70 level crosses the other way.
    #[test]
    fn crossing_down_through_a_high_level_fires_bearish() {
        let mut prices: Vec<f64> = (0..25).map(|i| 100.0 + i as f64 * 2.0).collect();
        for _ in 0..20 {
            prices.push(prices.last().unwrap() - 3.0);
        }
        let mut signal = RsiCross::new(14, 70.0).unwrap();
        let fired = run(&mut signal, &prices);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].direction, Direction::Bearish);
    }

    #[test]
    fn a_signal_shorter_than_its_warmup_never_fires() {
        let mut signal = RsiCross::new(14, 30.0).unwrap();
        assert!(run(&mut signal, &[100.0, 99.0, 98.0]).is_empty());
    }
}
