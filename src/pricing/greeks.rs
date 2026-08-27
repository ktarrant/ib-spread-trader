//! Greeks, which fall out once vol is known.
//!
//! IB has no historical greeks — `OPTION_IMPLIED_VOLATILITY` exists as a bar
//! series but there is no per-bar delta, ever. So delta here is derived: fit the
//! forward by parity, invert Black-76 for vol, then differentiate.
//!
//! These are **forward** greeks. With `discount_factor = 1.0` (see
//! [`super::parity`]) forward delta and spot delta differ only by the carry
//! basis, which the capture's own numbers put at +0.05–0.15% — well inside the
//! `delta_tolerance` any strike selector uses.

//! `clippy::neg_cmp_op_on_partial_ord` is allowed deliberately throughout this
//! module. `!(x > 0.0)` and `x <= 0.0` are **not** equivalent for `f64`: the
//! first is true for NaN and the second is false. Every negated comparison here
//! guards a degenerate or unsolvable input, and NaN is exactly the input it must
//! catch — rewriting them the way clippy suggests would let NaN through into the
//! pricer.
#![allow(clippy::neg_cmp_op_on_partial_ord)]

use super::black76::norm_cdf;

/// dPrice/dForward. Positive for calls, negative for puts.
///
/// Returns `None` on degenerate inputs (`T <= 0` or `vol <= 0`) rather than
/// snapping to the 0/±1 step function. At expiry the true delta is a
/// discontinuity, and a selector asking for 0.30 delta on an expired ladder
/// should get "no answer", not a strike chosen off the jump.
pub fn delta(forward: f64, strike: f64, t: f64, vol: f64, is_call: bool) -> Option<f64> {
    let denom = vol * t.max(0.0).sqrt();
    if !(denom > 0.0) || !(forward > 0.0) || !(strike > 0.0) {
        return None;
    }
    let d1 = ((forward / strike).ln() + 0.5 * vol * vol * t) / denom;
    Some(if is_call {
        norm_cdf(d1)
    } else {
        norm_cdf(d1) - 1.0
    })
}

#[cfg(test)]
mod tests {
    use super::super::black76::black76;
    use super::*;

    #[test]
    fn an_atm_call_is_near_half_delta() {
        let d = delta(100.0, 100.0, 0.25, 0.2, true).unwrap();
        assert!((d - 0.52).abs() < 0.02, "{d}");
    }

    /// The relationship that makes |delta| a usable selection key across both
    /// rights: a 0.30-delta call and a 0.70-delta put share a strike.
    #[test]
    fn call_and_put_delta_differ_by_exactly_one() {
        for k in [90.0, 100.0, 110.0] {
            let c = delta(100.0, k, 0.25, 0.2, true).unwrap();
            let p = delta(100.0, k, 0.25, 0.2, false).unwrap();
            assert!((c - p - 1.0).abs() < 1e-12, "K={k}: {c} - {p}");
        }
    }

    /// Deep wings saturate *exactly* at the bounds, and that is intended:
    /// `norm_cdf` floors to 0 past 37 sigma, so a strike 46 sigma out prices a
    /// put delta of precisely -1.0. What matters is that nothing ever escapes
    /// [0, 1] for calls or [-1, 0] for puts, because a selector comparing
    /// |delta| against a target must never see a value it cannot reach.
    #[test]
    fn deep_wings_saturate_at_the_bounds_without_crossing_them() {
        for (k, right, want) in [
            (1.0, true, 1.0),
            (10000.0, true, 0.0),
            (1.0, false, 0.0),
            (10000.0, false, -1.0),
        ] {
            let d = delta(100.0, k, 0.25, 0.2, right).unwrap();
            assert!(
                (d - want).abs() < 1e-6,
                "K={k} call={right}: {d}, want ~{want}"
            );
        }
        for k in [0.5, 1.0, 50.0, 100.0, 500.0, 10000.0] {
            assert!((0.0..=1.0).contains(&delta(100.0, k, 0.25, 0.2, true).unwrap()));
            assert!((-1.0..=0.0).contains(&delta(100.0, k, 0.25, 0.2, false).unwrap()));
        }
    }

    /// Delta is the price derivative, so a finite difference has to reproduce it.
    /// This is the check that would catch a sign slip or a missing term, which
    /// the closed-form assertions above would not.
    #[test]
    fn matches_a_finite_difference_of_the_price() {
        let (k, t, vol) = (105.0, 0.3, 0.28);
        for is_call in [true, false] {
            let h = 1e-5;
            let up = black76(100.0 + h, k, t, vol, is_call);
            let down = black76(100.0 - h, k, t, vol, is_call);
            let numeric = (up - down) / (2.0 * h);
            let analytic = delta(100.0, k, t, vol, is_call).unwrap();
            assert!((numeric - analytic).abs() < 1e-6, "{numeric} vs {analytic}");
        }
    }

    #[test]
    fn degenerate_inputs_have_no_delta() {
        assert_eq!(delta(100.0, 100.0, 0.0, 0.2, true), None);
        assert_eq!(delta(100.0, 100.0, 0.25, 0.0, true), None);
    }
}
