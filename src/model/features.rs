//! Per-bar feature extraction for the price-target model.
//!
//! The [`FeatureEngine`] is fed one **option bar** at a time, in order, and
//! carries its own indicator state — the same contract the signals keep. It is
//! deliberately driven from the 30-minute option-bar stream only (not the 5-min
//! stock-only bars): the ranked strategy decides only on option bars, and a
//! single resolution keeps a session-anchored VWAP from double-counting the
//! overlapping 30-min and 5-min volume and keeps ATR's ranges comparable.
//!
//! Every field the indicators produce is an [`Option`]: an indicator still
//! warming up, or a bar with no ATM IV / one-sided ladder, reports `None` rather
//! than a fabricated value. The price model decides how to fall back.

use crate::data::ChainSnapshot;
use crate::indicators::{Atr, Cross, CrossDetector, RealizedVol, Rsi, Sma, Vwap};
use anyhow::{Result, bail};
use chrono::Timelike;
use chrono_tz::America::New_York;

/// How far the model looks across the smile for skew, and the periods of the
/// trend/ATR/realized-vol indicators. Mapped from TOML in a later phase.
#[derive(Debug, Clone, Copy)]
pub struct FeatureConfig {
    pub trend_fast: usize,
    pub trend_slow: usize,
    pub atr_period: usize,
    pub realized_vol_period: usize,
    /// Winsorize each return to this many robust sigmas before measuring realized
    /// vol, so an overnight/weekend gap cannot inflate the forecast. `0` = off.
    pub realized_vol_winsor_k: f64,
    /// Log-moneyness magnitude at which put/call skew is measured (e.g. 0.05).
    pub skew_moneyness: f64,
    /// RSI lookback on the option-bar spot series, for the entry-timing gate.
    pub rsi_period: usize,
    /// The level whose upward crossing `bars_since_rsi_cross_up` counts from.
    pub rsi_cross_level: f64,
    /// Bars of ATM IV history behind the trailing median that `iv_vs_median` is
    /// measured against. A **trailing** window, never the whole sample: the whole
    /// sample includes the future, and a sizing rule that reads it is cheating.
    pub iv_median_lookback: usize,
}

impl Default for FeatureConfig {
    fn default() -> Self {
        Self {
            trend_fast: 10,
            trend_slow: 30,
            atr_period: 14,
            realized_vol_period: 20,
            realized_vol_winsor_k: 3.0,
            skew_moneyness: 0.05,
            rsi_period: 14,
            rsi_cross_level: 30.0,
            iv_median_lookback: 200,
        }
    }
}

/// A bar's distance to and from the nearest earnings prints, in calendar days.
/// Both `None` outside any earnings window; supplied by the engine, which owns
/// the earnings calendar.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct EarningsDistance {
    pub days_to: Option<i64>,
    pub days_since: Option<i64>,
}

/// One option bar's inputs to the price model. Prices are in the underlying's
/// units; `realized_vol`/`atm_iv`/`skew` are volatilities (per-bar for realized
/// vol — annualisation is the model's to apply).
/// A compact record of what the model *saw* at the bar it decided on, carried
/// onto the trade so a report can answer "what triggered this" and not only
/// "how confident was it". Serialization skips absent readings rather than
/// writing nulls, so a warming indicator is visibly absent.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct EntryFeatures {
    pub spot: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub trend: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vwap_distance: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub atr: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub realized_vol: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub atm_iv: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skew: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rsi: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub days_to_earnings: Option<i64>,
}

impl From<&Features> for EntryFeatures {
    fn from(f: &Features) -> Self {
        Self {
            spot: f.spot,
            trend: f.trend,
            vwap_distance: f.vwap_distance,
            atr: f.atr,
            realized_vol: f.realized_vol,
            atm_iv: f.atm_iv,
            skew: f.skew,
            rsi: f.rsi,
            days_to_earnings: f.days_to_earnings,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Features {
    /// Reference price (spot where quoted, else the parity forward).
    pub spot: f64,
    pub forward: f64,
    /// The parity fit's residual MAD — an earnings-stress proxy.
    pub residual_mad: f64,
    /// Calendar year fraction to expiry.
    pub year_fraction: f64,
    /// New York minute-of-day of the bar start, the time-of-day input.
    pub minute_of_day: u16,
    /// Fast-minus-slow SMA as a fraction of the slow SMA. Positive is an uptrend.
    pub trend: Option<f64>,
    /// `(spot - vwap) / vwap`; positive when trading above the session VWAP.
    pub vwap_distance: Option<f64>,
    /// Average true range, in price units.
    pub atr: Option<f64>,
    /// Per-bar realized volatility (population stddev of log returns).
    pub realized_vol: Option<f64>,
    pub atm_iv: Option<f64>,
    /// RSI of the spot series, `None` until the indicator is warm. Read by the
    /// entry-timing gate; the price model ignores it.
    pub rsi: Option<f64>,
    /// ATM IV over the median of the *previous* `iv_median_lookback` bars, so
    /// above 1.0 means this name is priced richer than it has been lately.
    /// `None` until half the window has been seen — a regime cannot be judged
    /// against a handful of readings.
    pub iv_vs_median: Option<f64>,
    /// Option bars since RSI last crossed **up** through `rsi_cross_level`, or
    /// `None` if it has not done so yet. Zero on the crossing bar itself.
    ///
    /// This is the difference between "oversold" and "recovering": the level
    /// gate opens while price is still falling, this one waits for the turn.
    pub bars_since_rsi_cross_up: Option<u32>,
    /// OTM put IV minus OTM call IV; positive is the usual equity skew.
    pub skew: Option<f64>,
    pub days_to_earnings: Option<i64>,
    pub days_since_earnings: Option<i64>,
}

/// Stateful extractor. Advance it once per option bar with [`observe`], which
/// returns that bar's [`Features`].
///
/// [`observe`]: FeatureEngine::observe
#[derive(Debug)]
pub struct FeatureEngine {
    trend_fast: Sma,
    trend_slow: Sma,
    atr: Atr,
    vwap: Vwap,
    realized_vol: RealizedVol,
    rsi: Rsi,
    iv_history: std::collections::VecDeque<f64>,
    iv_median_lookback: usize,
    rsi_cross: CrossDetector,
    rsi_cross_level: f64,
    bars_since_rsi_cross_up: Option<u32>,
    skew_moneyness: f64,
}

impl FeatureEngine {
    pub fn new(config: FeatureConfig) -> Result<Self> {
        if config.trend_fast >= config.trend_slow {
            bail!(
                "trend_fast ({}) must be shorter than trend_slow ({})",
                config.trend_fast,
                config.trend_slow
            );
        }
        if !config.skew_moneyness.is_finite() || config.skew_moneyness <= 0.0 {
            bail!("skew_moneyness must be positive");
        }
        Ok(Self {
            trend_fast: Sma::new(config.trend_fast)?,
            trend_slow: Sma::new(config.trend_slow)?,
            atr: Atr::new(config.atr_period)?,
            vwap: Vwap::new(),
            realized_vol: RealizedVol::with_winsor(
                config.realized_vol_period,
                config.realized_vol_winsor_k,
            )?,
            rsi: Rsi::new(config.rsi_period)?,
            iv_history: std::collections::VecDeque::with_capacity(config.iv_median_lookback),
            iv_median_lookback: config.iv_median_lookback.max(2),
            rsi_cross: CrossDetector::new(),
            rsi_cross_level: config.rsi_cross_level,
            bars_since_rsi_cross_up: None,
            skew_moneyness: config.skew_moneyness,
        })
    }

    /// Advance every indicator by one option bar and return that bar's features.
    ///
    /// Must be called **once per option bar, in order** — the indicators are
    /// stateful, so skipping or replaying a bar corrupts every later reading.
    /// Both averages are advanced unconditionally so the slow one always warms.
    pub fn observe(&mut self, chain: &ChainSnapshot, earnings: EarningsDistance) -> Features {
        let spot = chain.reference_price();
        let u = &chain.underlying;

        // Trend: advance both averages every bar; the gap is defined only once
        // the slow one is warm.
        let fast = self.trend_fast.next(spot);
        let slow = self.trend_slow.next(spot);
        let trend = fast
            .zip(slow)
            .and_then(|(f, s)| (s != 0.0).then_some((f - s) / s));

        // VWAP: a bar with no real volume contributes nothing but still yields the
        // running mean; OHLC fall back to spot, which the zero-volume path ignores.
        let vwap = self.vwap.next(
            chain.ts,
            u.high.unwrap_or(spot),
            u.low.unwrap_or(spot),
            u.close.unwrap_or(spot),
            u.volume.unwrap_or(0.0),
        );
        let vwap_distance = vwap.and_then(|w| (w != 0.0).then_some((spot - w) / w));

        let atr = self.atr.next(
            u.high.unwrap_or(spot),
            u.low.unwrap_or(spot),
            u.close.unwrap_or(spot),
        );
        let realized_vol = self.realized_vol.next(spot);
        // Advanced unconditionally, like the averages: an indicator fed only on
        // the bars someone happens to read never warms.
        let rsi = self.rsi.next(spot);
        // Age the counter first, then reset it if this bar is itself the cross,
        // so the crossing bar reports 0 rather than 1.
        self.bars_since_rsi_cross_up = self.bars_since_rsi_cross_up.map(|n| n.saturating_add(1));
        if let Some(v) = rsi
            && let Some(Cross::Up) = self.rsi_cross.update(v - self.rsi_cross_level)
        {
            self.bars_since_rsi_cross_up = Some(0);
        }
        let bars_since_rsi_cross_up = self.bars_since_rsi_cross_up;

        // Read the median of prior bars, then append — so a bar is never
        // compared against a window that already contains it.
        let atm_iv = chain.atm_implied_vol();
        let iv_vs_median = atm_iv.and_then(|iv| {
            let ready = self.iv_history.len() >= self.iv_median_lookback / 2;
            ready.then(|| {
                let mut sorted: Vec<f64> = self.iv_history.iter().copied().collect();
                sorted.sort_by(f64::total_cmp);
                let mid = sorted.len() / 2;
                let median = if sorted.len().is_multiple_of(2) {
                    0.5 * (sorted[mid - 1] + sorted[mid])
                } else {
                    sorted[mid]
                };
                (median > 0.0).then(|| iv / median)
            })
            .flatten()
        });
        if let Some(iv) = atm_iv {
            if self.iv_history.len() == self.iv_median_lookback {
                self.iv_history.pop_front();
            }
            self.iv_history.push_back(iv);
        }

        let minute_of_day = {
            let local = chain.ts.with_timezone(&New_York);
            (local.hour() * 60 + local.minute()) as u16
        };

        Features {
            spot,
            forward: chain.forward,
            residual_mad: chain.residual_mad,
            year_fraction: chain.year_fraction,
            minute_of_day,
            trend,
            vwap_distance,
            atr,
            realized_vol,
            atm_iv,
            iv_vs_median,
            rsi,
            bars_since_rsi_cross_up,
            skew: chain.put_call_skew(self.skew_moneyness),
            days_to_earnings: earnings.days_to,
            days_since_earnings: earnings.days_since,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{OptionQuote, UnderlyingBar};
    use crate::domain::OptionRight;
    use chrono::{Duration, TimeZone, Utc};

    /// A bar whose ladder carries a mild equity skew and a clean ATM point.
    fn bar(day: i64, spot: f64, volume: f64) -> ChainSnapshot {
        let quote = |strike: f64, right: OptionRight, iv: f64| OptionQuote {
            contract_id: (strike as i64) * 10 + i64::from(right.is_call()),
            strike,
            right,
            bid: 1.0,
            ask: 1.2,
            mid: 1.1,
            implied_vol: Some(iv),
            delta: Some(if right.is_call() { 0.4 } else { -0.4 }),
        };
        ChainSnapshot {
            symbol: "TEST".into(),
            expiry: chrono::NaiveDate::from_ymd_opt(2026, 8, 21).unwrap(),
            ts: Utc.with_ymd_and_hms(2026, 8, 3, 15, 0, 0).unwrap() + Duration::days(day),
            underlying: UnderlyingBar {
                mid: Some(spot),
                open: Some(spot),
                high: Some(spot * 1.01),
                low: Some(spot * 0.99),
                close: Some(spot),
                volume: Some(volume),
            },
            forward: spot,
            residual_mad: 0.02,
            paired_strikes: 4,
            year_fraction: 0.05,
            quotes: vec![
                quote(spot * 0.95, OptionRight::Put, 0.32),
                quote(spot * 0.90, OptionRight::Put, 0.36),
                quote(spot * 1.05, OptionRight::Call, 0.22),
                quote(spot * 1.10, OptionRight::Call, 0.20),
            ],
        }
    }

    fn compact_config() -> FeatureConfig {
        FeatureConfig {
            rsi_period: 14,
            rsi_cross_level: 30.0,
            iv_median_lookback: 200,
            trend_fast: 2,
            trend_slow: 3,
            atr_period: 2,
            realized_vol_period: 2,
            realized_vol_winsor_k: 3.0,
            skew_moneyness: 0.05,
        }
    }

    #[test]
    fn rejects_an_inverted_trend_pair_and_bad_skew_moneyness() {
        assert!(
            FeatureEngine::new(FeatureConfig {
                trend_fast: 5,
                trend_slow: 5,
                ..compact_config()
            })
            .is_err()
        );
        assert!(
            FeatureEngine::new(FeatureConfig {
                skew_moneyness: 0.0,
                ..compact_config()
            })
            .is_err()
        );
    }

    #[test]
    fn indicators_warm_up_then_report() {
        let mut engine = FeatureEngine::new(compact_config()).unwrap();

        // First bar: nothing that needs history is ready yet, but the snapshot
        // fields and skew are available immediately.
        let f0 = engine.observe(&bar(0, 100.0, 1_000.0), EarningsDistance::default());
        assert_eq!(f0.trend, None, "slow SMA not warm");
        assert_eq!(f0.realized_vol, None, "needs more closes");
        assert!(f0.atm_iv.is_some());
        assert!(f0.skew.unwrap() > 0.0, "puts richer than calls");
        assert_eq!(f0.minute_of_day, 11 * 60, "15:00 UTC is 11:00 New York");

        engine.observe(&bar(1, 102.0, 1_000.0), EarningsDistance::default());
        let f2 = engine.observe(&bar(2, 104.0, 1_000.0), EarningsDistance::default());
        assert!(f2.trend.unwrap() > 0.0, "rising closes read as an uptrend");
        assert!(f2.realized_vol.unwrap() > 0.0);
        assert!(f2.atr.unwrap() > 0.0);
    }

    #[test]
    fn vwap_distance_tracks_which_side_of_the_mean_price_sits() {
        let mut engine = FeatureEngine::new(compact_config()).unwrap();
        // First bar sets the session VWAP near 100; a jump to 110 the same day
        // (bar(0) again keeps the same NY date) sits above it.
        engine.observe(&bar(0, 100.0, 1_000.0), EarningsDistance::default());
        let mut high = bar(0, 110.0, 1_000.0);
        high.ts += Duration::minutes(30);
        let f = engine.observe(&high, EarningsDistance::default());
        assert!(f.vwap_distance.unwrap() > 0.0, "{:?}", f.vwap_distance);
    }

    #[test]
    fn earnings_distance_passes_through() {
        let mut engine = FeatureEngine::new(compact_config()).unwrap();
        let f = engine.observe(
            &bar(0, 100.0, 1_000.0),
            EarningsDistance {
                days_to: Some(3),
                days_since: Some(60),
            },
        );
        assert_eq!(f.days_to_earnings, Some(3));
        assert_eq!(f.days_since_earnings, Some(60));
    }
}
