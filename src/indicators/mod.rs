//! Indicators, kept incremental.
//!
//! Each one is fed a single value per bar and carries its own state. The parked
//! `algorithm-api` branch instead rebuilt its indicator from the whole close
//! history on every bar — correct, but O(n²), and this engine visits ~600 bars
//! per symbol per run.
//!
//! **Warm-up is suppressed explicitly, and this is not cosmetic.** `ta`'s RSI
//! returns a value from the very first `next()` call: 50.0 on bar one, computed
//! from no price change at all. Left alone that reading alone would fire a
//! "crossed up through 30" trigger at the start of every single backtest.

use anyhow::{Result, bail};
use ta::Next;
use ta::indicators::{RelativeStrengthIndex, SimpleMovingAverage};

/// Relative Strength Index, silent until it has seen enough bars to mean anything.
#[derive(Debug, Clone)]
pub struct Rsi {
    inner: RelativeStrengthIndex,
    period: usize,
    seen: usize,
}

impl Rsi {
    pub fn new(period: usize) -> Result<Self> {
        if period == 0 {
            bail!("RSI period must be at least 1");
        }
        Ok(Self {
            inner: RelativeStrengthIndex::new(period)
                .map_err(|e| anyhow::anyhow!("RSI period {period}: {e}"))?,
            period,
            seen: 0,
        })
    }

    /// Feed one bar. `None` until `period + 1` values have been seen — an
    /// n-period RSI needs n *changes*, and n changes need n+1 prices.
    pub fn next(&mut self, value: f64) -> Option<f64> {
        let reading = self.inner.next(value);
        self.seen += 1;
        (self.seen > self.period).then_some(reading)
    }
}

/// Simple moving average, silent until its window is full.
#[derive(Debug, Clone)]
pub struct Sma {
    inner: SimpleMovingAverage,
    period: usize,
    seen: usize,
}

impl Sma {
    pub fn new(period: usize) -> Result<Self> {
        if period == 0 {
            bail!("SMA period must be at least 1");
        }
        Ok(Self {
            inner: SimpleMovingAverage::new(period)
                .map_err(|e| anyhow::anyhow!("SMA period {period}: {e}"))?,
            period,
            seen: 0,
        })
    }

    /// Feed one bar. `None` until the window is full; before that `ta` averages
    /// fewer points than asked for and the result is not the indicator requested.
    pub fn next(&mut self, value: f64) -> Option<f64> {
        let reading = self.inner.next(value);
        self.seen += 1;
        (self.seen >= self.period).then_some(reading)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cross {
    Up,
    Down,
}

/// Fires when a quantity changes sign.
///
/// Feed it `value - reference`, which covers both shapes this crate needs: an
/// RSI against a fixed level, and a fast average against a slow one.
///
/// Only the last **non-zero** sign is remembered. A value sitting exactly on the
/// level is not a crossing, and treating it as one would fire twice — once on
/// the touch and once on the departure — for a single move through.
#[derive(Debug, Clone, Default)]
pub struct CrossDetector {
    last_sign: Option<i8>,
}

impl CrossDetector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn update(&mut self, delta: f64) -> Option<Cross> {
        let sign = if delta > 0.0 {
            1
        } else if delta < 0.0 {
            -1
        } else {
            return None;
        };

        let crossed = match self.last_sign {
            Some(previous) if previous != sign => {
                Some(if sign > 0 { Cross::Up } else { Cross::Down })
            }
            // The first reading establishes a baseline; it is not a crossing.
            _ => None,
        };
        self.last_sign = Some(sign);
        crossed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same fixture the parked `algorithm-api` branch used, so the two agree
    /// on the underlying crate's numbers.
    const FIXTURE: [f64; 17] = [
        44.0, 44.15, 43.9, 44.35, 44.8, 45.0, 44.7, 45.4, 45.75, 45.2, 45.9, 46.1, 45.8, 46.4,
        46.8, 46.5, 47.0,
    ];

    #[test]
    fn rsi_stays_silent_until_it_has_seen_enough_changes() {
        let mut rsi = Rsi::new(14).unwrap();
        for (i, value) in FIXTURE.iter().enumerate() {
            let reading = rsi.next(*value);
            if i + 1 <= 14 {
                assert_eq!(reading, None, "bar {} should be suppressed", i + 1);
            } else {
                assert!(reading.is_some(), "bar {} should report", i + 1);
            }
        }
    }

    #[test]
    fn rsi_matches_the_known_fixture_value_once_warm() {
        let mut rsi = Rsi::new(14).unwrap();
        let mut last = None;
        for value in FIXTURE {
            if let Some(reading) = rsi.next(value) {
                last = Some(reading);
            }
        }
        let value = last.unwrap();
        assert!((value - 72.81).abs() < 0.05, "{value}");
    }

    /// The specific spurious signal warm-up exists to prevent: `ta` reports 50.0
    /// on bar one, which sits above a 30 level and below a 70 one.
    #[test]
    fn the_unsuppressed_first_reading_would_have_been_a_false_signal() {
        use ta::Next;
        let mut raw = RelativeStrengthIndex::new(14).unwrap();
        assert_eq!(raw.next(44.0), 50.0, "computed from no price change at all");

        let mut guarded = Rsi::new(14).unwrap();
        assert_eq!(guarded.next(44.0), None);
    }

    #[test]
    fn sma_stays_silent_until_its_window_is_full() {
        let mut sma = Sma::new(3).unwrap();
        assert_eq!(sma.next(1.0), None);
        assert_eq!(sma.next(2.0), None);
        assert_eq!(sma.next(3.0), Some(2.0));
        assert_eq!(sma.next(4.0), Some(3.0));
    }

    #[test]
    fn zero_periods_are_rejected() {
        assert!(Rsi::new(0).is_err());
        assert!(Sma::new(0).is_err());
    }

    #[test]
    fn the_first_reading_is_a_baseline_not_a_crossing() {
        let mut d = CrossDetector::new();
        assert_eq!(d.update(5.0), None);
        assert_eq!(d.update(6.0), None, "still on the same side");
        assert_eq!(d.update(-1.0), Some(Cross::Down));
    }

    #[test]
    fn crossings_fire_once_per_direction_change() {
        let mut d = CrossDetector::new();
        d.update(-5.0);
        assert_eq!(d.update(2.0), Some(Cross::Up));
        assert_eq!(d.update(3.0), None);
        assert_eq!(d.update(-1.0), Some(Cross::Down));
        assert_eq!(d.update(-2.0), None);
    }

    /// A value landing exactly on the level is a touch, not a crossing. Counting
    /// it would fire twice for one move through — once arriving, once leaving.
    #[test]
    fn touching_the_level_exactly_is_not_a_crossing() {
        let mut d = CrossDetector::new();
        d.update(-5.0);
        assert_eq!(d.update(0.0), None, "arriving at the level");
        assert_eq!(
            d.update(3.0),
            Some(Cross::Up),
            "and the move through fires once"
        );

        let mut back = CrossDetector::new();
        back.update(-5.0);
        assert_eq!(back.update(0.0), None);
        assert_eq!(back.update(-2.0), None, "came back the way it arrived");
    }
}
