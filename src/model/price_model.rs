//! The price-target engine: features in, a distribution over the underlying at a
//! horizon out.
//!
//! [`PriceModel`] is the seam. The ranker only ever sees a [`PriceDistribution`],
//! so a richer model — one that reads option flow, or shapes the tails from the
//! whole smile — drops in here without the ranker or the engine changing.
//!
//! The v1 model, [`LognormalDriftModel`], is a lognormal in price: a volatility
//! blended from realized vol and ATM IV, scaled to the horizon, with a small
//! drift from trend, VWAP mean-reversion, and skew. It is intentionally close to
//! a random walk unless its weights are turned up — the honest null the ranker
//! ranks departures from.

use super::distribution::PriceDistribution;
use super::features::Features;

/// Turns a bar's [`Features`] into a distribution over the underlying price
/// `horizon_years` from now. Look-ahead is impossible: the model only ever sees
/// the features of the bar it is called on.
pub trait PriceModel {
    fn name(&self) -> &str;

    /// The forecast horizon this model was configured and validated against.
    fn horizon_days(&self) -> f64;

    fn forecast(&self, features: &Features, horizon_years: f64) -> PriceDistribution;

    /// The random-walk null this model is scored against: no drift, and vol =
    /// ATM IV — the option market's own forecast, untouched. A model that
    /// reshapes the width must not reshape its null too, or the eval would
    /// compare the model with itself.
    fn null_forecast(&self, features: &Features, horizon_years: f64) -> PriceDistribution {
        self.forecast(&null_features(features), horizon_years)
    }

    /// The options' implied vol at the horizon as a multiple of today's — how
    /// the ranker revalues each leg once the horizon is reached. 1.0 holds every
    /// leg at its entry vol (sticky strike).
    fn horizon_vol_scale(&self, _features: &Features, _horizon_years: f64) -> f64 {
        1.0
    }
}

/// Whether an earnings release falls inside a forecast of `horizon_years`.
fn earnings_inside(features: &Features, horizon_years: f64) -> bool {
    features
        .days_to_earnings
        .is_some_and(|d| d >= 0 && d as f64 <= horizon_years * 365.25)
}

/// A bar's features with every drift and non-IV vol input removed.
pub(crate) fn null_features(features: &Features) -> Features {
    Features {
        trend: None,
        vwap_distance: None,
        skew: None,
        realized_vol: None,
        atr: None,
        ..*features
    }
}

/// Tunable weights for [`LognormalDriftModel`]. Mapped from TOML in a later phase.
#[derive(Debug, Clone, Copy)]
pub struct LognormalModelConfig {
    /// Forecast horizon in calendar days, capped at expiry by the engine.
    pub horizon_days: f64,
    /// Bars per year, to annualise the per-bar realized vol (30-min bars ≈ 3276).
    pub bars_per_year: f64,
    /// Weight on annualised realized vol in the vol blend; `1 - w` on ATM IV.
    pub realized_vol_weight: f64,
    /// Annualised vol to fall back to when neither realized vol nor ATM IV is
    /// available (early bars, or a bar with no clean smile).
    pub fallback_vol: f64,
    /// Horizon log-drift per unit of trend (fast/slow SMA gap).
    pub trend_weight: f64,
    /// Horizon log-drift per unit of VWAP distance, applied as mean-reversion
    /// (trading above VWAP nudges the drift down).
    pub vwap_reversion: f64,
    /// Horizon log-drift per unit of put/call skew (richer downside nudges the
    /// median down). A first-order skew effect; tail shaping is a future model.
    pub skew_weight: f64,
    /// Half-width of the price grid, in horizon sigmas.
    pub grid_sigma: f64,
    /// Number of grid points (odd keeps a point on the median).
    pub grid_points: usize,
    /// Scales the whole volatility: below 1 says ATM IV overstates the moves
    /// that follow (the variance premium option sellers collect).
    pub vol_multiplier: f64,
    /// Further scales the volatility when an earnings release falls inside the
    /// horizon. A monthly chain's IV spreads the release's jump over the whole
    /// month, so a week that contains it moves more than that IV implies.
    pub earnings_vol_multiplier: f64,
    /// Revalue the legs at the symbol's trailing-median IV, not their entry IV,
    /// when an earnings release falls inside the horizon — the crush. Sticky
    /// strike across a release overvalues every long-vol position and
    /// undervalues every short one, because the event's premium does not
    /// survive the event. Never raises a vol: a bar already below its median
    /// keeps its own.
    pub crush_to_median: bool,
    /// Keep the trend drift only when it points the same way as the prevailing
    /// trend: fade a spike against the trend, hold no view on one with it (or
    /// while the prevailing trend is still warming).
    pub trend_with_prevailing_only: bool,
}

impl Default for LognormalModelConfig {
    fn default() -> Self {
        Self {
            // One calendar week: a forecast made on a given weekday is scored
            // against the same weekday next week.
            horizon_days: 7.0,
            bars_per_year: 3276.0,
            realized_vol_weight: 0.5,
            fallback_vol: 0.30,
            trend_weight: 1.0,
            vwap_reversion: 0.0,
            skew_weight: 0.5,
            grid_sigma: 4.0,
            grid_points: 101,
            vol_multiplier: 1.0,
            earnings_vol_multiplier: 1.0,
            crush_to_median: false,
            trend_with_prevailing_only: false,
        }
    }
}

#[derive(Debug)]
pub struct LognormalDriftModel {
    name: String,
    config: LognormalModelConfig,
}

impl LognormalDriftModel {
    pub fn new(config: LognormalModelConfig) -> Self {
        Self {
            name: "lognormal_drift".to_string(),
            config,
        }
    }

    /// Annualised volatility for this bar: the configured blend of realized vol
    /// and ATM IV, each used alone if the other is missing, else the fallback.
    fn annual_vol(&self, features: &Features) -> f64 {
        let realized = features
            .realized_vol
            .map(|rv| rv * self.config.bars_per_year.sqrt());
        let iv = features.atm_iv;
        let w = self.config.realized_vol_weight;
        match (realized, iv) {
            (Some(r), Some(v)) => w * r + (1.0 - w) * v,
            (Some(r), None) => r,
            (None, Some(v)) => v,
            (None, None) => self.config.fallback_vol,
        }
    }

    /// Horizon log-drift: `E[S_h] = spot · exp(drift)`.
    fn drift(&self, features: &Features) -> f64 {
        let trend = self.config.trend_weight * features.trend.unwrap_or(0.0);
        let agrees = features.prevailing_trend.is_some_and(|p| p * trend > 0.0);
        let trend = if self.config.trend_with_prevailing_only && !agrees { 0.0 } else { trend };
        trend
            - self.config.vwap_reversion * features.vwap_distance.unwrap_or(0.0)
            - self.config.skew_weight * features.skew.unwrap_or(0.0)
    }
}

impl PriceModel for LognormalDriftModel {
    fn name(&self) -> &str {
        &self.name
    }

    fn horizon_days(&self) -> f64 {
        self.config.horizon_days
    }

    fn forecast(&self, features: &Features, horizon_years: f64) -> PriceDistribution {
        let scale = self.config.vol_multiplier
            * if earnings_inside(features, horizon_years) {
                self.config.earnings_vol_multiplier
            } else {
                1.0
            };
        self.distribution(features, horizon_years, scale)
    }

    fn horizon_vol_scale(&self, features: &Features, horizon_years: f64) -> f64 {
        if !(self.config.crush_to_median && earnings_inside(features, horizon_years)) {
            return 1.0;
        }
        features
            .iv_vs_median
            .filter(|ratio| *ratio > 0.0)
            .map_or(1.0, |ratio| (1.0 / ratio).min(1.0))
    }

    fn null_forecast(&self, features: &Features, horizon_years: f64) -> PriceDistribution {
        self.distribution(&null_features(features), horizon_years, 1.0)
    }
}

impl LognormalDriftModel {
    /// The lognormal at `vol_scale` × the configured vol.
    fn distribution(&self, features: &Features, horizon_years: f64, vol_scale: f64) -> PriceDistribution {
        let spot = features.spot;
        let sigma_annual = self.annual_vol(features) * vol_scale;

        // Scale vol to the horizon, then floor it so the grid never collapses:
        // a per-bar ATR range is a sane minimum, and a hard epsilon guards the
        // degenerate bar with nothing at all.
        let atr_floor = features
            .atr
            .filter(|_| spot > 0.0)
            .map(|a| a / spot)
            .unwrap_or(0.0);
        let sigma = (sigma_annual * horizon_years.max(0.0).sqrt())
            .max(atr_floor)
            .max(1e-4);

        let drift = self.drift(features);
        // Lognormal: ln(S_h / spot) ~ Normal(m, sigma) with m = drift - σ²/2, so
        // that the mean of S_h is spot·exp(drift) rather than the median.
        let m = drift - 0.5 * sigma * sigma;

        let n = self.config.grid_points.max(3);
        let half = self.config.grid_sigma.max(1.0) * sigma;
        let (lo, hi) = (m - half, m + half);

        let mut prices = Vec::with_capacity(n);
        let mut weights = Vec::with_capacity(n);
        for i in 0..n {
            let x = lo + (hi - lo) * (i as f64) / ((n - 1) as f64);
            let z = (x - m) / sigma;
            weights.push((-0.5 * z * z).exp());
            prices.push(spot * x.exp());
        }

        PriceDistribution::from_weights(prices, weights)
            .expect("a non-degenerate grid always has positive mass")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spread::{Leg, Side, Spread};
    use crate::domain::OptionRight;

    fn features(trend: Option<f64>) -> Features {
        Features {
            rsi: None,
            iv_vs_median: None,
            bars_since_rsi_cross_up: None,
            spot: 100.0,
            forward: 100.0,
            residual_mad: 0.02,
            year_fraction: 0.1,
            minute_of_day: 660,
            trend,
            prevailing_trend: None,
            vwap_distance: None,
            atr: None,
            realized_vol: None,
            atm_iv: Some(0.20),
            skew: None,
            days_to_earnings: None,
            days_since_earnings: None,
        }
    }

    /// Across a release the legs revert to the symbol's own median IV: a bar at
    /// 1.25× its median revalues at 0.8×. Never upward, never without a release,
    /// never when the model does not ask for it.
    #[test]
    fn the_crush_reverts_to_the_median_only_across_a_release() {
        let model = |crush| LognormalDriftModel::new(LognormalModelConfig {
            crush_to_median: crush,
            ..Default::default()
        });
        let at = |days: Option<i64>, ratio: Option<f64>| Features {
            days_to_earnings: days,
            iv_vs_median: ratio,
            ..features(None)
        };
        let week = 7.0 / 365.25;
        assert!((model(true).horizon_vol_scale(&at(Some(3), Some(1.25)), week) - 0.8).abs() < 1e-12);
        assert_eq!(model(true).horizon_vol_scale(&at(Some(3), Some(0.9)), week), 1.0, "never upward");
        assert_eq!(model(true).horizon_vol_scale(&at(Some(12), Some(1.25)), week), 1.0, "past the horizon");
        assert_eq!(model(true).horizon_vol_scale(&at(None, Some(1.25)), week), 1.0, "no release");
        assert_eq!(model(false).horizon_vol_scale(&at(Some(3), Some(1.25)), week), 1.0, "not asked");
    }

    /// Fading a spike up (bearish drift) is kept in a falling prevailing trend
    /// and dropped in a rising one or an unknown one.
    #[test]
    fn the_fade_is_kept_only_when_it_agrees_with_the_prevailing_trend() {
        let model = |only| LognormalDriftModel::new(LognormalModelConfig {
            trend_weight: -0.5,
            trend_with_prevailing_only: only,
            ..Default::default()
        });
        let spike_up = |prevailing| Features { prevailing_trend: prevailing, ..features(Some(0.02)) };
        assert_eq!(model(true).drift(&spike_up(Some(-0.01))), -0.01, "against the trend: fade");
        assert_eq!(model(true).drift(&spike_up(Some(0.01))), 0.0, "with the trend: no view");
        assert_eq!(model(true).drift(&spike_up(None)), 0.0, "warming: no view");
        assert_eq!(model(false).drift(&spike_up(Some(0.01))), -0.01, "off: always fade");
    }

    #[test]
    fn a_driftless_forecast_is_centered_on_spot_and_integrates_to_one() {
        let model = LognormalDriftModel::new(LognormalModelConfig {
            trend_weight: 0.0,
            skew_weight: 0.0,
            ..Default::default()
        });
        let dist = model.forecast(&features(None), 0.05);
        assert!(
            (dist.probs().iter().sum::<f64>() - 1.0).abs() < 1e-12,
            "probabilities must sum to one"
        );
        // With zero drift the mean is spot; the discretised grid is close.
        assert!((dist.mean() - 100.0).abs() < 0.5, "mean {}", dist.mean());
    }

    #[test]
    fn a_positive_trend_shifts_the_mean_up() {
        let model = LognormalDriftModel::new(LognormalModelConfig {
            trend_weight: 1.0,
            skew_weight: 0.0,
            ..Default::default()
        });
        let flat = model.forecast(&features(None), 0.05).mean();
        let rising = model.forecast(&features(Some(0.02)), 0.05).mean();
        assert!(rising > flat + 1.0, "flat {flat}, rising {rising}");
    }

    #[test]
    fn a_wider_horizon_widens_the_distribution() {
        let model = LognormalDriftModel::new(LognormalModelConfig::default());
        let near = model.forecast(&features(None), 0.02).stddev();
        let far = model.forecast(&features(None), 0.20).stddev();
        assert!(far > near, "near {near}, far {far}");
    }

    /// The property the ranker leans on: E[payoff] integrates the payoff against
    /// the grid, and must equal the hand sum over the atoms.
    #[test]
    fn expected_payoff_of_a_vertical_matches_the_manual_sum() {
        let model = LognormalDriftModel::new(LognormalModelConfig::default());
        let dist = model.forecast(&features(None), 0.05);
        let spread = Spread::new(
            "TEST",
            vec![
                Leg {
                    contract_id: 1,
                    strike: 100.0,
                    right: OptionRight::Call,
                    side: Side::Long,
                    quantity: 1,
                },
                Leg {
                    contract_id: 2,
                    strike: 105.0,
                    right: OptionRight::Call,
                    side: Side::Short,
                    quantity: 1,
                },
            ],
        );

        let via_expected = dist.expected(|s| spread.payoff_at(s));
        let manual: f64 = dist
            .prices()
            .iter()
            .zip(dist.probs())
            .map(|(&s, &p)| p * spread.payoff_at(s))
            .sum();
        assert!((via_expected - manual).abs() < 1e-9);
        // A 100/105 call spread is worth something between 0 and its $500 width.
        assert!(via_expected > 0.0 && via_expected < 500.0, "{via_expected}");
    }
}
