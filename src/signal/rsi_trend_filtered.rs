//! RSI crossing a level, gated by a trend filter.
//!
//! The classic oversold-bounce strategy fires too often in a downtrend. This
//! signal only lets an RSI crossing through when the broader trend agrees:
//! - **Price > SMA** mode: the close must be above a single SMA.
//! - **Fast > Slow SMA** mode: the fast SMA must be above the slow SMA.
//!
//! Bearish crossings are gated by the inverse (price < SMA, or fast < slow).

use super::{MarketView, Signal, Trigger, fire};
use crate::indicators::{CrossDetector, Rsi, Sma};
use anyhow::{Result, bail};

/// How the trend is measured.
#[derive(Debug)]
enum TrendMode {
    /// Price must be above (bullish) or below (bearish) this SMA.
    PriceVsSma(Sma),
    /// The fast SMA must be above (bullish) or below (bearish) the slow SMA.
    SmaCross { fast: Sma, slow: Sma },
}

#[derive(Debug)]
pub struct RsiTrendFiltered {
    name: String,
    level: f64,
    rsi: Rsi,
    detector: CrossDetector,
    trend: TrendMode,
    filtered_count: usize,
}

impl RsiTrendFiltered {
    /// Price-vs-SMA mode: the crossing only fires when price is on the right
    /// side of the SMA.
    pub fn with_price_sma(rsi_period: usize, level: f64, trend_period: usize) -> Result<Self> {
        Ok(Self {
            name: format!("rsi_trend(rsi={rsi_period}, level={level}, sma={trend_period})"),
            level,
            rsi: Rsi::new(rsi_period)?,
            detector: CrossDetector::new(),
            trend: TrendMode::PriceVsSma(Sma::new(trend_period)?),
            filtered_count: 0,
        })
    }

    /// Fast-vs-slow SMA mode: the crossing only fires when the fast SMA is on
    /// the right side of the slow SMA.
    pub fn with_sma_cross(
        rsi_period: usize,
        level: f64,
        fast_period: usize,
        slow_period: usize,
    ) -> Result<Self> {
        if fast_period >= slow_period {
            bail!("trend fast period ({fast_period}) must be shorter than slow ({slow_period})");
        }
        Ok(Self {
            name: format!(
                "rsi_trend(rsi={rsi_period}, level={level}, sma={fast_period}/{slow_period})"
            ),
            level,
            rsi: Rsi::new(rsi_period)?,
            detector: CrossDetector::new(),
            trend: TrendMode::SmaCross {
                fast: Sma::new(fast_period)?,
                slow: Sma::new(slow_period)?,
            },
            filtered_count: 0,
        })
    }

}

impl Signal for RsiTrendFiltered {
    fn name(&self) -> &str {
        &self.name
    }

    fn evaluate(&mut self, view: &MarketView<'_>) -> Option<Trigger> {
        let price = view.price();

        // Advance all indicators unconditionally — starving any of them would
        // corrupt later readings.
        let rsi_reading = self.rsi.next(price);

        let trend_bullish = match &mut self.trend {
            TrendMode::PriceVsSma(sma) => {
                let sma_val = sma.next(price);
                sma_val.map(|sv| price > sv)
            }
            TrendMode::SmaCross { fast, slow } => {
                let f = fast.next(price);
                let s = slow.next(price);
                match (f, s) {
                    (Some(fv), Some(sv)) => Some(fv > sv),
                    _ => None,
                }
            }
        };

        let rsi_val = rsi_reading?;
        let cross = self.detector.update(rsi_val - self.level)?;

        let is_bullish = cross == crate::indicators::Cross::Up;
        match trend_bullish {
            Some(bullish) if bullish == is_bullish => {
                Some(fire(view.chain.ts, cross, (rsi_val - self.level).abs()))
            }
            Some(_) | None => {
                self.filtered_count += 1;
                None
            }
        }
    }

    fn diagnostics(&self) -> std::collections::BTreeMap<String, usize> {
        let mut m = std::collections::BTreeMap::new();
        m.insert("trend_filtered".to_string(), self.filtered_count);
        m
    }
}

#[cfg(test)]
mod tests {
    use super::super::Direction;
    use super::super::testing::run;
    use super::*;

    /// An uptrend, then a sharp dip, then recovery — the realistic scenario
    /// for a trend-filtered RSI. The SMAs stay bullish through the dip because
    /// the prior uptrend keeps them elevated.
    fn uptrend_dip_recovery() -> Vec<f64> {
        let mut prices: Vec<f64> = (0..30).map(|i| 100.0 + i as f64 * 0.5).collect();
        for _ in 0..10 {
            prices.push(prices.last().unwrap() - 2.0);
        }
        for _ in 0..15 {
            prices.push(prices.last().unwrap() + 1.5);
        }
        prices
    }

    /// A pure V from a low base — price stays below a long SMA throughout
    /// the recovery, so a bullish trend filter should suppress the crossing.
    fn v_shape_from_low() -> Vec<f64> {
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
    fn fires_bullish_when_trend_agrees_price_vs_sma() {
        // SMA(5) tracks the recovery tightly enough that price > SMA when
        // the RSI crosses back up through 30.
        let mut signal = RsiTrendFiltered::with_price_sma(14, 30.0, 5).unwrap();
        let fired = run(&mut signal, &uptrend_dip_recovery());
        let bullish: Vec<_> = fired
            .iter()
            .filter(|t| t.direction == Direction::Bullish)
            .collect();
        assert_eq!(bullish.len(), 1, "the oversold bounce should fire");
    }

    #[test]
    fn suppressed_when_trend_disagrees() {
        // In a pure V from a low base, price stays below the 30-bar SMA
        // throughout the recovery.
        let mut signal = RsiTrendFiltered::with_price_sma(14, 30.0, 30).unwrap();
        let fired = run(&mut signal, &v_shape_from_low());
        let bullish: Vec<_> = fired
            .iter()
            .filter(|t| t.direction == Direction::Bullish)
            .collect();
        assert!(bullish.is_empty(), "long SMA should filter the bullish bounce");
        assert!(signal.filtered_count > 0);
    }

    #[test]
    fn longer_sma_filters_more_aggressively() {
        // SMA(10) is still too high at the exact RSI crossing in the
        // uptrend-dip-recovery, proving the filter is selective.
        let mut signal = RsiTrendFiltered::with_price_sma(14, 30.0, 10).unwrap();
        let fired = run(&mut signal, &uptrend_dip_recovery());
        let bullish: Vec<_> = fired
            .iter()
            .filter(|t| t.direction == Direction::Bullish)
            .collect();
        assert!(
            bullish.is_empty(),
            "SMA(10) lags far enough to filter the bounce"
        );
    }

    #[test]
    fn fires_bullish_when_fast_above_slow_sma() {
        // Fast(3)/slow(5): the fast SMA recovers before the slow after the dip.
        let mut signal = RsiTrendFiltered::with_sma_cross(14, 30.0, 3, 5).unwrap();
        let fired = run(&mut signal, &uptrend_dip_recovery());
        let bullish: Vec<_> = fired
            .iter()
            .filter(|t| t.direction == Direction::Bullish)
            .collect();
        assert_eq!(bullish.len(), 1, "the oversold bounce should fire");
    }

    #[test]
    fn a_flat_tape_fires_nothing() {
        let mut signal = RsiTrendFiltered::with_price_sma(14, 30.0, 20).unwrap();
        assert!(run(&mut signal, &[100.0; 60]).is_empty());
    }

    #[test]
    fn inverted_sma_periods_are_rejected() {
        assert!(RsiTrendFiltered::with_sma_cross(14, 30.0, 20, 5).is_err());
        assert!(RsiTrendFiltered::with_sma_cross(14, 30.0, 5, 5).is_err());
    }

    #[test]
    fn diagnostics_count_filtered_signals() {
        let mut signal = RsiTrendFiltered::with_price_sma(14, 30.0, 40).unwrap();
        run(&mut signal, &v_shape_from_low());
        let diag = signal.diagnostics();
        assert!(
            diag["trend_filtered"] > 0,
            "the suppressed crossing should be counted"
        );
    }
}
