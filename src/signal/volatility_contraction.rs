//! Volatility contraction followed by an underlying breakout.
//!
//! The contraction belongs to the **previous** completed bar. The breakout bar
//! itself is allowed to widen the bands; using its post-breakout volatility to
//! decide whether a squeeze existed would erase the setup after observing it.

use super::{Direction, MarketView, Signal, Trigger};
use anyhow::{Result, bail};
use chrono::Timelike;
use chrono_tz::America::New_York;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContractionMeasure {
    #[default]
    BollingerWidth,
    RealizedVolatility,
}

#[derive(Debug, Clone, Copy)]
pub struct VolatilityContractionParams {
    pub measure: ContractionMeasure,
    pub volatility_period: usize,
    pub contraction_lookback: usize,
    pub contraction_percentile: f64,
    pub breakout_period: usize,
    pub volume_lookback_sessions: usize,
    pub min_volume_ratio: f64,
    pub max_atm_iv_increase: f64,
}

impl Default for VolatilityContractionParams {
    fn default() -> Self {
        Self {
            measure: ContractionMeasure::BollingerWidth,
            volatility_period: 20,
            contraction_lookback: 120,
            contraction_percentile: 0.10,
            breakout_period: 20,
            volume_lookback_sessions: 20,
            min_volume_ratio: 1.50,
            max_atm_iv_increase: 0.10,
        }
    }
}

#[derive(Debug)]
pub struct VolatilityContractionBreakout {
    name: String,
    params: VolatilityContractionParams,
    closes: VecDeque<f64>,
    highs: VecDeque<f64>,
    contraction_history: VecDeque<f64>,
    previous_compressed: Option<bool>,
    volume_by_slot: BTreeMap<u16, VecDeque<f64>>,
    previous_atm_iv: Option<f64>,
    breakout_active: bool,
    diagnostics: BTreeMap<String, usize>,
}

impl VolatilityContractionBreakout {
    pub fn new(params: VolatilityContractionParams) -> Result<Self> {
        if params.volatility_period < 2 {
            bail!("volatility_period must be at least 2");
        }
        if params.contraction_lookback < 2 {
            bail!("contraction_lookback must be at least 2");
        }
        if !params.contraction_percentile.is_finite()
            || !(0.0..=1.0).contains(&params.contraction_percentile)
        {
            bail!("contraction_percentile must be between 0 and 1");
        }
        if params.breakout_period == 0 {
            bail!("breakout_period must be at least 1");
        }
        if params.volume_lookback_sessions == 0 {
            bail!("volume_lookback_sessions must be at least 1");
        }
        if !params.min_volume_ratio.is_finite() || params.min_volume_ratio <= 0.0 {
            bail!("min_volume_ratio must be positive");
        }
        if !params.max_atm_iv_increase.is_finite() || params.max_atm_iv_increase < 0.0 {
            bail!("max_atm_iv_increase must be non-negative");
        }

        Ok(Self {
            name: format!(
                "volatility_contraction_breakout({:?}, {}, {})",
                params.measure, params.volatility_period, params.breakout_period
            ),
            params,
            closes: VecDeque::new(),
            highs: VecDeque::new(),
            contraction_history: VecDeque::new(),
            previous_compressed: None,
            volume_by_slot: BTreeMap::new(),
            previous_atm_iv: None,
            breakout_active: false,
            diagnostics: BTreeMap::new(),
        })
    }

    fn count(&mut self, key: &str) {
        *self.diagnostics.entry(key.to_string()).or_default() += 1;
    }

    fn close_window_size(&self) -> usize {
        self.params.volatility_period
            + usize::from(self.params.measure == ContractionMeasure::RealizedVolatility)
    }

    fn current_measure(&self) -> Option<f64> {
        if self.closes.len() < self.close_window_size() {
            return None;
        }
        match self.params.measure {
            ContractionMeasure::BollingerWidth => {
                let values: Vec<f64> = self.closes.iter().copied().collect();
                let mean = values.iter().sum::<f64>() / values.len() as f64;
                if mean <= 0.0 {
                    return None;
                }
                let variance = values
                    .iter()
                    .map(|value| (value - mean).powi(2))
                    .sum::<f64>()
                    / values.len() as f64;
                Some(4.0 * variance.sqrt() / mean)
            }
            ContractionMeasure::RealizedVolatility => {
                let closes: Vec<f64> = self.closes.iter().copied().collect();
                crate::indicators::realized_vol(&closes)
            }
        }
    }

    fn volume_baseline(&self, slot: u16) -> Option<f64> {
        self.volume_by_slot
            .get(&slot)
            .filter(|values| values.len() == self.params.volume_lookback_sessions)
            .and_then(|values| percentile(values, 0.5))
    }

    fn advance(&mut self, close: f64, high: f64, volume: f64, slot: u16, atm_iv: Option<f64>) {
        let close_window_size = self.close_window_size();
        push_bounded(&mut self.closes, close, close_window_size);
        if let Some(measure) = self.current_measure() {
            self.previous_compressed =
                if self.contraction_history.len() == self.params.contraction_lookback {
                    percentile(
                        &self.contraction_history,
                        self.params.contraction_percentile,
                    )
                    .map(|threshold| measure <= threshold)
                } else {
                    None
                };
            push_bounded(
                &mut self.contraction_history,
                measure,
                self.params.contraction_lookback,
            );
        } else {
            self.previous_compressed = None;
        }

        push_bounded(&mut self.highs, high, self.params.breakout_period);
        push_bounded(
            self.volume_by_slot.entry(slot).or_default(),
            volume,
            self.params.volume_lookback_sessions,
        );
        self.previous_atm_iv = atm_iv;
    }
}

impl Signal for VolatilityContractionBreakout {
    fn name(&self) -> &str {
        &self.name
    }

    fn evaluate(&mut self, view: &MarketView<'_>) -> Option<Trigger> {
        self.count("bars_seen");

        let underlying = view.chain.underlying;
        let stock = match (underlying.high, underlying.close, underlying.volume) {
            (Some(high), Some(close), Some(volume))
                if high.is_finite()
                    && close.is_finite()
                    && volume.is_finite()
                    && close > 0.0
                    && high > 0.0
                    && volume > 0.0 =>
            {
                Some((high, close, volume))
            }
            _ => None,
        };
        let atm_iv = view
            .chain
            .atm_implied_vol()
            .filter(|value| value.is_finite() && *value > 0.0);

        if stock.is_none() {
            self.count("missing_underlying");
        }
        if atm_iv.is_none() {
            self.count("missing_atm_iv");
        }

        let mut trigger = None;
        if let Some((high, close, volume)) = stock {
            let local = view.chain.ts.with_timezone(&New_York);
            let slot = (local.hour() * 60 + local.minute()) as u16;
            let prior_high = (self.highs.len() == self.params.breakout_period)
                .then(|| self.highs.iter().copied().reduce(f64::max))
                .flatten();
            let volume_baseline = self.volume_baseline(slot);
            let iv_ok = self.previous_atm_iv.zip(atm_iv).map(|(previous, current)| {
                current / previous <= 1.0 + self.params.max_atm_iv_increase
            });

            let ready = self.previous_compressed.is_some()
                && prior_high.is_some()
                && volume_baseline.is_some()
                && iv_ok.is_some();
            if ready {
                self.count("bars_ready");
                let compressed = self.previous_compressed == Some(true);
                let breakout = close > prior_high.expect("checked above");
                let breakout_edge = breakout && !self.breakout_active;
                let volume_ok = volume
                    >= self.params.min_volume_ratio * volume_baseline.expect("checked above");

                if compressed {
                    self.count("contraction_pass");
                    if breakout_edge {
                        self.count("breakout_pass");
                        if volume_ok {
                            self.count("volume_pass");
                            if iv_ok == Some(true) {
                                self.count("iv_pass");
                                trigger = Some(Trigger {
                                    ts: view.chain.ts,
                                    direction: Direction::Bullish,
                                    strength: close / prior_high.expect("checked above") - 1.0,
                                });
                            }
                        }
                    }
                }
                self.breakout_active = breakout;
            } else {
                self.breakout_active = false;
            }

            self.advance(close, high, volume, slot, atm_iv);
        } else {
            // Preserve the fact that the immediately preceding option bar lacked
            // an IV reading; the next bar must not compare against stale IV.
            self.previous_atm_iv = atm_iv;
            self.breakout_active = false;
        }

        trigger
    }

    fn diagnostics(&self) -> BTreeMap<String, usize> {
        self.diagnostics.clone()
    }
}

fn push_bounded(values: &mut VecDeque<f64>, value: f64, capacity: usize) {
    if values.len() == capacity {
        values.pop_front();
    }
    values.push_back(value);
}

fn percentile(values: &VecDeque<f64>, quantile: f64) -> Option<f64> {
    let mut sorted: Vec<f64> = values.iter().copied().filter(|v| v.is_finite()).collect();
    if sorted.is_empty() {
        return None;
    }
    sorted.sort_by(f64::total_cmp);
    let rank = quantile * (sorted.len() - 1) as f64;
    let lower = rank.floor() as usize;
    let upper = rank.ceil() as usize;
    let weight = rank - lower as f64;
    Some(sorted[lower] + weight * (sorted[upper] - sorted[lower]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::{ChainSnapshot, OptionQuote, UnderlyingBar};
    use crate::domain::OptionRight;
    use chrono::{Duration, TimeZone, Utc};

    fn bar(day: i64, close: f64, high: f64, volume: f64, atm_iv: f64) -> ChainSnapshot {
        let quote = |contract_id, strike, right| OptionQuote {
            contract_id,
            strike,
            right,
            bid: 1.0,
            ask: 1.1,
            mid: 1.05,
            implied_vol: Some(atm_iv),
            delta: Some(if right.is_call() { 0.5 } else { -0.5 }),
        };
        ChainSnapshot {
            symbol: "TEST".into(),
            ts: Utc.with_ymd_and_hms(2026, 7, 1, 15, 0, 0).unwrap() + Duration::days(day),
            underlying: UnderlyingBar {
                mid: Some(close),
                open: Some(close),
                high: Some(high),
                low: Some(close.min(high)),
                close: Some(close),
                volume: Some(volume),
            },
            forward: close,
            residual_mad: 0.0,
            paired_strikes: 2,
            year_fraction: 0.1,
            quotes: vec![
                quote(1, close * 0.99, OptionRight::Put),
                quote(2, close * 1.01, OptionRight::Call),
            ],
        }
    }

    fn compact_params() -> VolatilityContractionParams {
        VolatilityContractionParams {
            volatility_period: 3,
            contraction_lookback: 3,
            contraction_percentile: 0.10,
            breakout_period: 2,
            volume_lookback_sessions: 2,
            min_volume_ratio: 1.50,
            max_atm_iv_increase: 0.10,
            ..Default::default()
        }
    }

    fn setup_path(final_iv: f64) -> Vec<ChainSnapshot> {
        vec![
            bar(0, 100.0, 101.0, 100.0, 0.20),
            bar(1, 104.0, 105.0, 100.0, 0.20),
            bar(2, 96.0, 97.0, 100.0, 0.20),
            bar(3, 104.0, 105.0, 100.0, 0.20),
            bar(4, 100.0, 101.0, 100.0, 0.20),
            // This is the compressed bar. The next bar consumes its flag.
            bar(5, 100.0, 101.0, 100.0, 0.20),
            // Its own wide move is deliberately not part of the squeeze test.
            bar(6, 110.0, 111.0, 200.0, final_iv),
        ]
    }

    #[test]
    fn percentiles_interpolate_without_looking_outside_the_window() {
        let values = VecDeque::from(vec![4.0, 1.0, 3.0, 2.0]);
        assert_eq!(percentile(&values, 0.0), Some(1.0));
        assert_eq!(percentile(&values, 0.5), Some(2.5));
        assert_eq!(percentile(&values, 1.0), Some(4.0));
    }

    #[test]
    fn invalid_parameters_are_rejected() {
        let invalid = [
            VolatilityContractionParams {
                volatility_period: 1,
                ..Default::default()
            },
            VolatilityContractionParams {
                contraction_percentile: 1.1,
                ..Default::default()
            },
            VolatilityContractionParams {
                volume_lookback_sessions: 0,
                ..Default::default()
            },
            VolatilityContractionParams {
                max_atm_iv_increase: -0.1,
                ..Default::default()
            },
        ];
        assert!(
            invalid
                .into_iter()
                .all(|params| VolatilityContractionBreakout::new(params).is_err())
        );
    }

    #[test]
    fn the_previous_bars_squeeze_can_fire_on_a_wide_breakout_bar() {
        let mut signal = VolatilityContractionBreakout::new(compact_params()).unwrap();
        let fired: Vec<Trigger> = setup_path(0.21)
            .iter()
            .filter_map(|chain| {
                signal.evaluate(&MarketView {
                    symbol: "TEST",
                    chain,
                })
            })
            .collect();

        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].direction, Direction::Bullish);
        assert_eq!(fired[0].ts, setup_path(0.21).last().unwrap().ts);
        assert!(fired[0].strength > 0.08);
        let diagnostics = signal.diagnostics();
        assert_eq!(diagnostics["breakout_pass"], 1);
        assert_eq!(diagnostics["volume_pass"], 1);
        assert_eq!(diagnostics["iv_pass"], 1);
    }

    #[test]
    fn an_atm_iv_explosion_vetoes_the_otherwise_identical_breakout() {
        let mut signal = VolatilityContractionBreakout::new(compact_params()).unwrap();
        let fired: Vec<Trigger> = setup_path(0.23)
            .iter()
            .filter_map(|chain| {
                signal.evaluate(&MarketView {
                    symbol: "TEST",
                    chain,
                })
            })
            .collect();

        assert!(fired.is_empty());
        let diagnostics = signal.diagnostics();
        assert_eq!(diagnostics["breakout_pass"], 1);
        assert_eq!(diagnostics["volume_pass"], 1);
        assert!(!diagnostics.contains_key("iv_pass"));
    }

    #[test]
    fn a_missing_previous_atm_iv_is_not_replaced_with_a_stale_value() {
        let mut path = setup_path(0.21);
        path[5].quotes.clear();
        let mut signal = VolatilityContractionBreakout::new(compact_params()).unwrap();

        let fired: Vec<Trigger> = path
            .iter()
            .filter_map(|chain| {
                signal.evaluate(&MarketView {
                    symbol: "TEST",
                    chain,
                })
            })
            .collect();

        assert!(fired.is_empty());
        assert_eq!(signal.diagnostics()["missing_atm_iv"], 1);
    }

    #[test]
    fn realized_volatility_uses_the_requested_number_of_returns() {
        let mut params = compact_params();
        params.measure = ContractionMeasure::RealizedVolatility;
        let mut signal = VolatilityContractionBreakout::new(params).unwrap();
        signal.closes = VecDeque::from(vec![100.0, 102.0, 101.0]);
        assert_eq!(
            signal.current_measure(),
            None,
            "three returns need four closes"
        );
        signal.closes.push_back(103.0);
        assert!(signal.current_measure().unwrap() > 0.0);
    }

    #[test]
    fn volume_baselines_do_not_mix_the_open_with_midday() {
        let mut signal = VolatilityContractionBreakout::new(compact_params()).unwrap();
        signal
            .volume_by_slot
            .insert(9 * 60 + 30, VecDeque::from(vec![1_000.0, 1_200.0]));
        signal
            .volume_by_slot
            .insert(12 * 60, VecDeque::from(vec![100.0, 120.0]));

        assert_eq!(signal.volume_baseline(9 * 60 + 30), Some(1_100.0));
        assert_eq!(signal.volume_baseline(12 * 60), Some(110.0));
    }
}
