//! A discrete probability distribution over the underlying price at a horizon.
//!
//! Deliberately dumb: a sorted grid of prices and their probabilities. The price
//! model builds it; the ranker integrates a spread's payoff against it. Keeping
//! it model-agnostic means a richer model later produces the same shape and the
//! ranker never changes.

/// A normalized grid: `prices` ascending, `probs` summing to 1.
#[derive(Debug, Clone)]
pub struct PriceDistribution {
    prices: Vec<f64>,
    probs: Vec<f64>,
}

impl PriceDistribution {
    /// Build from a price grid and non-negative, unnormalized weights, dividing
    /// through by their total. `None` if the grid is empty, mismatched, or the
    /// weights sum to zero (a distribution with no mass is not a distribution).
    pub fn from_weights(prices: Vec<f64>, weights: Vec<f64>) -> Option<Self> {
        if prices.is_empty() || prices.len() != weights.len() {
            return None;
        }
        let total: f64 = weights.iter().sum();
        if !total.is_finite() || total <= 0.0 {
            return None;
        }
        let probs = weights.iter().map(|w| w / total).collect();
        Some(Self { prices, probs })
    }

    /// Expected value of `f` over the distribution: `Σ pᵢ · f(priceᵢ)`. This is
    /// the one operation the ranker needs — pass it a spread's payoff.
    pub fn expected<F: Fn(f64) -> f64>(&self, f: F) -> f64 {
        self.prices
            .iter()
            .zip(&self.probs)
            .map(|(&price, &p)| p * f(price))
            .sum()
    }

    pub fn mean(&self) -> f64 {
        self.expected(|s| s)
    }

    pub fn variance(&self) -> f64 {
        let mean = self.mean();
        self.expected(|s| (s - mean).powi(2))
    }

    pub fn stddev(&self) -> f64 {
        self.variance().sqrt()
    }

    /// Probability mass strictly below `x`.
    pub fn prob_below(&self, x: f64) -> f64 {
        self.prices
            .iter()
            .zip(&self.probs)
            .filter(|&(&price, _)| price < x)
            .map(|(_, &p)| p)
            .sum()
    }

    pub fn prob_above(&self, x: f64) -> f64 {
        1.0 - self.prob_below(x)
    }

    /// Probability mass in `[lo, hi)`.
    pub fn prob_between(&self, lo: f64, hi: f64) -> f64 {
        self.prices
            .iter()
            .zip(&self.probs)
            .filter(|&(&price, _)| lo <= price && price < hi)
            .map(|(_, &p)| p)
            .sum()
    }

    /// The CDF at `x`: probability mass at or below `x`. `F(realized)` is the
    /// probability integral transform (PIT) used to test calibration.
    pub fn cdf(&self, x: f64) -> f64 {
        self.prices
            .iter()
            .zip(&self.probs)
            .filter(|&(&price, _)| price <= x)
            .map(|(_, &p)| p)
            .sum()
    }

    /// Continuous Ranked Probability Score against a realized value `y`, in the
    /// energy form `E|X − y| − ½·E|X − X'|`. Lower is better; it rewards a
    /// forecast for putting mass near the outcome and for being sharp. For a
    /// point-mass forecast it reduces to `|mean − y|`, which the tests pin.
    pub fn crps(&self, y: f64) -> f64 {
        let expected_abs_error: f64 = self
            .prices
            .iter()
            .zip(&self.probs)
            .map(|(&x, &p)| p * (x - y).abs())
            .sum();
        let mut expected_spread = 0.0;
        for (&xi, &pi) in self.prices.iter().zip(&self.probs) {
            for (&xj, &pj) in self.prices.iter().zip(&self.probs) {
                expected_spread += pi * pj * (xi - xj).abs();
            }
        }
        expected_abs_error - 0.5 * expected_spread
    }

    /// The `q`-quantile (0..=1) by cumulative mass, returning the grid price at
    /// which the running total first reaches `q`.
    pub fn quantile(&self, q: f64) -> f64 {
        let mut cumulative = 0.0;
        for (&price, &p) in self.prices.iter().zip(&self.probs) {
            cumulative += p;
            if cumulative >= q {
                return price;
            }
        }
        *self.prices.last().expect("distribution is non-empty")
    }

    pub fn prices(&self) -> &[f64] {
        &self.prices
    }

    pub fn probs(&self) -> &[f64] {
        &self.probs
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weights_are_normalized_to_probabilities() {
        let d = PriceDistribution::from_weights(vec![10.0, 20.0], vec![3.0, 1.0]).unwrap();
        assert_eq!(d.probs(), &[0.75, 0.25]);
        assert!((d.probs().iter().sum::<f64>() - 1.0).abs() < 1e-12);
    }

    #[test]
    fn a_degenerate_grid_is_rejected() {
        assert!(PriceDistribution::from_weights(vec![], vec![]).is_none());
        assert!(PriceDistribution::from_weights(vec![1.0], vec![0.0]).is_none());
        assert!(PriceDistribution::from_weights(vec![1.0, 2.0], vec![1.0]).is_none());
    }

    #[test]
    fn mean_and_variance_match_a_hand_calc() {
        // 0.75 at 10, 0.25 at 20: mean = 12.5, var = 0.75*6.25 + 0.25*56.25 = 18.75.
        let d = PriceDistribution::from_weights(vec![10.0, 20.0], vec![3.0, 1.0]).unwrap();
        assert!((d.mean() - 12.5).abs() < 1e-12);
        assert!((d.variance() - 18.75).abs() < 1e-12);
        assert!((d.expected(|s| s) - d.mean()).abs() < 1e-12);
    }

    #[test]
    fn crps_of_a_point_mass_is_the_absolute_error() {
        let point = PriceDistribution::from_weights(vec![100.0], vec![1.0]).unwrap();
        assert!((point.crps(103.0) - 3.0).abs() < 1e-12);
        assert!((point.crps(100.0) - 0.0).abs() < 1e-12);
    }

    #[test]
    fn crps_rewards_putting_mass_near_the_outcome() {
        // Same spread, but one distribution is centered on the outcome.
        let near = PriceDistribution::from_weights(vec![99.0, 101.0], vec![1.0, 1.0]).unwrap();
        let far = PriceDistribution::from_weights(vec![109.0, 111.0], vec![1.0, 1.0]).unwrap();
        assert!(near.crps(100.0) < far.crps(100.0));
    }

    #[test]
    fn cdf_is_the_cumulative_mass_at_or_below() {
        let d =
            PriceDistribution::from_weights(vec![10.0, 20.0, 30.0], vec![1.0, 1.0, 2.0]).unwrap();
        assert!((d.cdf(9.0) - 0.0).abs() < 1e-12);
        assert!((d.cdf(20.0) - 0.5).abs() < 1e-12);
        assert!((d.cdf(30.0) - 1.0).abs() < 1e-12);
    }

    #[test]
    fn tail_masses_and_quantiles_read_off_the_grid() {
        let d =
            PriceDistribution::from_weights(vec![10.0, 20.0, 30.0], vec![1.0, 1.0, 2.0]).unwrap();
        assert!((d.prob_below(25.0) - 0.5).abs() < 1e-12);
        assert!((d.prob_above(25.0) - 0.5).abs() < 1e-12);
        assert!((d.prob_between(15.0, 35.0) - 0.75).abs() < 1e-12);
        assert_eq!(d.quantile(0.5), 20.0, "median crosses at the middle atom");
        assert_eq!(d.quantile(0.6), 30.0);
    }
}
