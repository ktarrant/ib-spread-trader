//! Recovering the forward from a strike ladder, by put-call parity.
//!
//! The capture stores no historical greeks and — before schema v6 — no intraday
//! underlying either. What it does store is dense bid/ask bars across a whole
//! strike ladder, and at each timestamp every strike quoted on both sides gives
//! an independent read on the forward:
//!
//! ```text
//! F = K + C - P
//! ```
//!
//! The median across the ladder is the estimate. Implied vol computed on it is
//! free of any rate or dividend assumption, because the carry is already inside F.
//!
//! **Why the median and not a regression.** For European options `C - P = DF*(F-K)`
//! is a straight line in K, so least squares would recover the forward and the
//! discount factor together from slope and intercept. These are American equity
//! options and it does not work: fitted on the real snapshot, 93% of timestamps
//! return DF > 1, which is unphysical, and the residual is worse than the
//! median's (MAD 0.065 vs 0.027 on SPY). The early-exercise premium bends the
//! line by far more than the ~0.1% of carry the slope is trying to measure.
//! This was established in `~/ib-option-viz` against real data — do not
//! reintroduce the regression without re-checking those numbers.

/// Strikes this close to the money carry the least early-exercise premium.
const ATM_BAND: f64 = 0.10;
/// Below this many strikes in the band, the restricted median is noisier than
/// the unrestricted one and the first pass stands.
const MIN_BAND_STRIKES: usize = 4;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParityFit {
    pub forward: f64,
    /// Always 1.0 — see the module docs. Kept in the struct so a European-style
    /// chain could carry a real one without changing callers.
    pub discount_factor: f64,
    /// Median absolute deviation of `K + C - P` around the forward. This is the
    /// fit's own confession: it roughly triples in the bar an earnings print
    /// lands in, as market makers reprice and a few strikes quote stale.
    pub residual_mad: f64,
}

/// One paired strike: both sides of the market quoted.
#[derive(Debug, Clone, Copy)]
pub struct PairedStrike {
    pub strike: f64,
    pub call_mid: f64,
    pub put_mid: f64,
}

/// Two-pass median of `K + C - P`.
///
/// The first pass over every paired strike gives a provisional forward; the
/// second restricts to strikes within [`ATM_BAND`] of it, where the American
/// early-exercise premium is smallest. Falls back to the first pass when the
/// band is too thin to trust.
///
/// Returns `None` on an empty ladder — there is no forward to report, and a NaN
/// would propagate silently into every downstream vol.
pub fn parity_fit(ladder: &[PairedStrike]) -> Option<ParityFit> {
    if ladder.is_empty() {
        return None;
    }

    let mut implied: Vec<f64> = ladder
        .iter()
        .map(|s| s.strike + s.call_mid - s.put_mid)
        .collect();
    let mut forward = median(&mut implied);

    let mut near: Vec<f64> = ladder
        .iter()
        .filter(|s| (s.strike / forward - 1.0).abs() <= ATM_BAND)
        .map(|s| s.strike + s.call_mid - s.put_mid)
        .collect();
    if near.len() >= MIN_BAND_STRIKES {
        forward = median(&mut near);
    }

    let mut deviations: Vec<f64> = implied.iter().map(|v| (v - forward).abs()).collect();
    let residual_mad = median(&mut deviations);

    Some(ParityFit {
        forward,
        discount_factor: 1.0,
        residual_mad,
    })
}

/// Median by partial sort. Takes `&mut` because it reorders in place; every
/// caller here owns its scratch vector.
fn median(values: &mut [f64]) -> f64 {
    debug_assert!(!values.is_empty());
    values.sort_by(|a, b| a.partial_cmp(b).expect("parity inputs are finite"));
    let n = values.len();
    if n % 2 == 1 {
        values[n / 2]
    } else {
        0.5 * (values[n / 2 - 1] + values[n / 2])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const EPS: f64 = 1e-9;

    fn ladder(rows: &[(f64, f64, f64)]) -> Vec<PairedStrike> {
        rows.iter()
            .map(|&(strike, call_mid, put_mid)| PairedStrike {
                strike,
                call_mid,
                put_mid,
            })
            .collect()
    }

    /// Fixtures and expected values produced by running
    /// `optionviz.pricing.parity_fit` on the same ladders.
    #[test]
    fn matches_the_python_reference_on_a_noisy_ladder() {
        let l = ladder(&[
            (745.0, 12.10, 3.52),
            (748.0, 10.05, 4.38),
            (750.0, 8.75, 5.10),
            (752.0, 7.55, 5.94),
            (755.0, 5.90, 7.21),
            (758.0, 4.45, 8.83),
            (760.0, 3.60, 10.04),
        ]);
        let fit = parity_fit(&l).unwrap();
        assert!((fit.forward - 753.62).abs() < EPS, "{}", fit.forward);
        assert!((fit.residual_mad - 0.03999999999996362).abs() < 1e-9);
        assert_eq!(fit.discount_factor, 1.0);
    }

    /// The point of the second pass. This ladder's wings carry a fat
    /// early-exercise premium, so the unrestricted median lands at 754.425;
    /// restricting to the ATM band pulls it to 753.67, matching the near-money
    /// strikes that actually agree with each other.
    #[test]
    fn the_atm_band_pass_moves_the_forward_off_the_biased_wings() {
        let l = ladder(&[
            (600.0, 155.0, 0.6),
            (650.0, 106.0, 1.4),
            (700.0, 58.0, 3.2),
            (745.0, 12.10, 3.52),
            (750.0, 8.75, 5.10),
            (755.0, 5.90, 7.21),
            (760.0, 3.60, 10.04),
            (800.0, 0.45, 46.00),
            (850.0, 0.05, 95.00),
            (900.0, 0.01, 145.0),
        ]);
        let fit = parity_fit(&l).unwrap();
        assert!(
            (fit.forward - 753.6700000000001).abs() < EPS,
            "{}",
            fit.forward
        );
        assert!((fit.residual_mad - 0.7549999999999386).abs() < 1e-9);

        // And the first pass on its own would have landed elsewhere.
        let mut all: Vec<f64> = l
            .iter()
            .map(|s| s.strike + s.call_mid - s.put_mid)
            .collect();
        let unrestricted = median(&mut all);
        assert!((unrestricted - 754.425).abs() < EPS, "{unrestricted}");
    }

    #[test]
    fn a_ladder_too_thin_for_the_band_keeps_the_first_pass() {
        let l = ladder(&[
            (750.0, 8.75, 5.10),
            (755.0, 5.90, 7.21),
            (760.0, 3.60, 10.04),
        ]);
        let fit = parity_fit(&l).unwrap();
        assert!((fit.forward - 753.65).abs() < EPS, "{}", fit.forward);
        assert!((fit.residual_mad - 0.04000000000007731).abs() < 1e-9);
    }

    #[test]
    fn an_empty_ladder_has_no_forward() {
        assert!(parity_fit(&[]).is_none());
    }

    /// A single paired strike is a degenerate but legitimate fit: F = K + C - P
    /// exactly, with nothing to disagree with it.
    #[test]
    fn a_single_strike_fits_exactly_with_zero_residual() {
        let fit = parity_fit(&ladder(&[(750.0, 8.75, 5.10)])).unwrap();
        assert!((fit.forward - 753.65).abs() < EPS);
        assert_eq!(fit.residual_mad, 0.0);
    }
}
