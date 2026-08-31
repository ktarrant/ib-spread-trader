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
use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::America::New_York;
use std::collections::VecDeque;
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

/// Drop the oldest value once `capacity` is reached, then append.
fn push_bounded(values: &mut VecDeque<f64>, value: f64, capacity: usize) {
    if values.len() == capacity {
        values.pop_front();
    }
    values.push_back(value);
}

/// Population standard deviation of the log returns across `closes`, per bar
/// (not annualised). This is the realized-volatility measure the
/// volatility-contraction signal uses, factored out so the `RealizedVol`
/// indicator and that signal compute it identically.
///
/// `None` if fewer than two prices, or any price is non-positive — a log return
/// through zero is undefined, and defaulting it to something would invent a move
/// the tape never showed.
pub fn realized_vol(closes: &[f64]) -> Option<f64> {
    realized_vol_winsorized(closes, 0.0)
}

/// Realized volatility with each return **winsorized** to `k` robust sigmas
/// before the stddev is taken, so a single jump — an overnight or weekend gap —
/// cannot masquerade as sustained diffusion volatility and blow the estimate up.
///
/// The scale is a MAD around the median return, which the outlier itself barely
/// moves, so the cap is set by the *typical* return and the gap is pulled back to
/// it. `k <= 0` disables winsorizing and reproduces [`realized_vol`] exactly.
pub fn realized_vol_winsorized(closes: &[f64], k: f64) -> Option<f64> {
    if closes.len() < 2 {
        return None;
    }
    let returns: Option<Vec<f64>> = closes
        .windows(2)
        .map(|w| (w[0] > 0.0 && w[1] > 0.0).then(|| (w[1] / w[0]).ln()))
        .collect();
    let mut returns = returns?;

    if k > 0.0 {
        let median = median_of(&returns);
        let deviations: Vec<f64> = returns.iter().map(|r| (r - median).abs()).collect();
        // 1.4826 makes the MAD a consistent estimator of the standard deviation
        // for Gaussian data.
        let scale = 1.4826 * median_of(&deviations);
        if scale > 0.0 {
            let cap = k * scale;
            for r in &mut returns {
                *r = median + (*r - median).clamp(-cap, cap);
            }
        }
    }

    let mean = returns.iter().sum::<f64>() / returns.len() as f64;
    let variance = returns
        .iter()
        .map(|value| (value - mean).powi(2))
        .sum::<f64>()
        / returns.len() as f64;
    Some(variance.sqrt())
}

/// Median of a slice, by value. Copies and sorts; the windows here are small.
fn median_of(values: &[f64]) -> f64 {
    let mut sorted: Vec<f64> = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let n = sorted.len();
    if n == 0 {
        0.0
    } else if n % 2 == 1 {
        sorted[n / 2]
    } else {
        0.5 * (sorted[n / 2 - 1] + sorted[n / 2])
    }
}

/// Realized volatility over a rolling window, silent until the window is full.
///
/// `period` is the number of **returns** in the window, so it holds `period + 1`
/// closes — an n-return measure needs n+1 prices, the same n-vs-n+1 accounting as
/// [`Rsi`]. The reading is the per-bar stddev of log returns; annualisation is
/// the caller's to apply, since the bar interval is not the indicator's to know.
#[derive(Debug, Clone)]
pub struct RealizedVol {
    closes: VecDeque<f64>,
    period: usize,
    /// Winsorize each return to this many robust sigmas; `0` = no winsorizing.
    winsor_k: f64,
}

impl RealizedVol {
    pub fn new(period: usize) -> Result<Self> {
        Self::with_winsor(period, 0.0)
    }

    /// As [`new`](Self::new), but winsorizing each return to `winsor_k` robust
    /// sigmas so a gap cannot inflate the estimate (see [`realized_vol_winsorized`]).
    pub fn with_winsor(period: usize, winsor_k: f64) -> Result<Self> {
        if period == 0 {
            bail!("realized-vol period must be at least 1");
        }
        Ok(Self {
            closes: VecDeque::with_capacity(period + 1),
            period,
            winsor_k,
        })
    }

    /// Feed one close. `None` until `period + 1` closes have been seen, or if any
    /// close in the window is non-positive (see [`realized_vol`]).
    pub fn next(&mut self, close: f64) -> Option<f64> {
        push_bounded(&mut self.closes, close, self.period + 1);
        if self.closes.len() < self.period + 1 {
            return None;
        }
        let window: Vec<f64> = self.closes.iter().copied().collect();
        realized_vol_winsorized(&window, self.winsor_k)
    }
}

/// Average True Range: the SMA of the true range, silent until the window is full.
///
/// True range is `max(high-low, |high-prev_close|, |low-prev_close|)`; the first
/// bar has no previous close, so its true range is just `high-low`. The averaging
/// is a plain SMA rather than Wilder's smoothing — it reuses [`Sma`]'s warm-up
/// suppression, and this crate does not need the extra pinning a second smoothing
/// scheme would demand.
#[derive(Debug, Clone)]
pub struct Atr {
    true_range: Sma,
    prev_close: Option<f64>,
}

impl Atr {
    pub fn new(period: usize) -> Result<Self> {
        Ok(Self {
            true_range: Sma::new(period)?,
            prev_close: None,
        })
    }

    /// Feed one bar's high/low/close. `None` until `period` bars have been seen.
    pub fn next(&mut self, high: f64, low: f64, close: f64) -> Option<f64> {
        let tr = match self.prev_close {
            Some(prev) => (high - low)
                .max((high - prev).abs())
                .max((low - prev).abs()),
            None => high - low,
        };
        self.prev_close = Some(close);
        self.true_range.next(tr)
    }
}

/// Session-anchored VWAP: the volume-weighted average of the typical price since
/// the current New York trading session began.
///
/// It resets when the New York calendar date changes, so each session is priced
/// against its own open rather than dragging yesterday's mean forward. Typical
/// price is `(high + low + close) / 3`. Bars with non-positive volume contribute
/// nothing — underlying volume comes only from `trades` rows, and a bid/ask row's
/// sentinel volume must not enter the mean.
///
/// The reading is `None` until the session has accumulated any real volume.
#[derive(Debug, Clone, Default)]
pub struct Vwap {
    session: Option<NaiveDate>,
    cumulative_pv: f64,
    cumulative_volume: f64,
}

impl Vwap {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn next(
        &mut self,
        ts: DateTime<Utc>,
        high: f64,
        low: f64,
        close: f64,
        volume: f64,
    ) -> Option<f64> {
        let day = ts.with_timezone(&New_York).date_naive();
        if self.session != Some(day) {
            self.session = Some(day);
            self.cumulative_pv = 0.0;
            self.cumulative_volume = 0.0;
        }
        if volume > 0.0 && volume.is_finite() {
            let typical = (high + low + close) / 3.0;
            self.cumulative_pv += typical * volume;
            self.cumulative_volume += volume;
        }
        (self.cumulative_volume > 0.0).then(|| self.cumulative_pv / self.cumulative_volume)
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
            if i < 14 {
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

    /// Two symmetric returns put the population stddev at half their spread, a
    /// value that can be checked by hand independent of the loop above.
    #[test]
    fn realized_vol_is_the_population_stddev_of_log_returns() {
        let got = realized_vol(&[100.0, 110.0, 105.0]).unwrap();
        let r1: f64 = (110.0f64 / 100.0).ln();
        let r2: f64 = (105.0f64 / 110.0).ln();
        assert!((got - (r1 - r2).abs() / 2.0).abs() < 1e-12, "{got}");
    }

    #[test]
    fn winsorizing_tames_a_single_gap() {
        // Nine quiet moves and one 10% gap.
        let returns = [0.005, -0.004, 0.006, -0.005, 0.004, -0.006, 0.003, -0.004, 0.005, 0.10];
        let mut closes = vec![100.0];
        for r in returns {
            closes.push(closes.last().unwrap() * (1.0 + r));
        }
        let raw = realized_vol(&closes).unwrap();
        let tamed = realized_vol_winsorized(&closes, 3.0).unwrap();
        assert!(tamed < raw, "raw {raw}, tamed {tamed}");
        assert!(tamed < 0.02, "the gap no longer dominates: {tamed}");
        // k <= 0 reproduces the raw estimate exactly.
        assert_eq!(realized_vol_winsorized(&closes, 0.0), realized_vol(&closes));
    }

    #[test]
    fn realized_vol_refuses_short_or_non_positive_windows() {
        assert_eq!(realized_vol(&[100.0]), None, "needs at least two prices");
        assert_eq!(
            realized_vol(&[100.0, -5.0, 105.0]),
            None,
            "a return through zero is undefined"
        );
    }

    #[test]
    fn realized_vol_indicator_stays_silent_until_its_window_is_full() {
        let mut rv = RealizedVol::new(2).unwrap();
        assert_eq!(rv.next(100.0), None);
        assert_eq!(rv.next(110.0), None, "two returns need three closes");
        let reading = rv.next(105.0).unwrap();
        assert!((reading - realized_vol(&[100.0, 110.0, 105.0]).unwrap()).abs() < 1e-12);
    }

    #[test]
    fn atr_averages_true_range_and_suppresses_warm_up() {
        let mut atr = Atr::new(2).unwrap();
        assert_eq!(atr.next(10.0, 8.0, 9.0), None, "window not full");
        // TR = max(11-9, |11-9|, |9-9|) = 2; window [2, 2].
        assert_eq!(atr.next(11.0, 9.0, 10.0), Some(2.0));
        // TR = max(12-9, |12-10|, |9-10|) = 3; window [2, 3].
        assert_eq!(atr.next(12.0, 9.0, 11.0), Some(2.5));
    }

    #[test]
    fn vwap_accumulates_within_a_session_and_resets_across_days() {
        use chrono::TimeZone;
        let mut vwap = Vwap::new();
        let day1_open = Utc.with_ymd_and_hms(2026, 8, 19, 15, 0, 0).unwrap();
        let day1_late = Utc.with_ymd_and_hms(2026, 8, 19, 15, 30, 0).unwrap();
        let day2 = Utc.with_ymd_and_hms(2026, 8, 20, 15, 0, 0).unwrap();

        assert_eq!(vwap.next(day1_open, 10.0, 8.0, 9.0, 100.0), Some(9.0));
        // (900 + 11*300) / 400 = 10.5
        assert_eq!(vwap.next(day1_late, 12.0, 10.0, 11.0, 300.0), Some(10.5));
        // A new session forgets the prior mean entirely.
        assert_eq!(vwap.next(day2, 20.0, 18.0, 19.0, 50.0), Some(19.0));
    }

    #[test]
    fn vwap_ignores_sentinel_volume() {
        use chrono::TimeZone;
        let mut vwap = Vwap::new();
        let ts = Utc.with_ymd_and_hms(2026, 8, 19, 15, 0, 0).unwrap();
        assert_eq!(vwap.next(ts, 10.0, 8.0, 9.0, 0.0), None, "no real volume yet");
        assert_eq!(vwap.next(ts, 10.0, 8.0, 9.0, 100.0), Some(9.0));
        assert_eq!(
            vwap.next(ts, 50.0, 40.0, 45.0, 0.0),
            Some(9.0),
            "a zero-volume bar cannot move the mean"
        );
    }
}
