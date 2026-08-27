//! Time to expiry.
//!
//! Equity options stop trading at the 16:00 **Eastern** close on expiration
//! Friday. That has to be resolved through a real timezone rather than a fixed
//! UTC offset: the capture window straddles no DST boundary today, but a chain
//! captured across early November would put the expiry an hour wrong, and near
//! expiry an hour is ~4% of the remaining year fraction — enough to visibly move
//! delta, and therefore enough to move which strike a selector picks.

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use chrono_tz::America::New_York;

const YEAR_SECONDS: f64 = 365.25 * 24.0 * 3600.0;

/// The instant an expiration-date option stops trading: 16:00 America/New_York.
pub fn expiry_instant(expiry: NaiveDate) -> Result<DateTime<Utc>> {
    let naive = expiry
        .and_hms_opt(16, 0, 0)
        .context("16:00 is a valid wall-clock time")?;
    // `.single()` is None only on a DST spring-forward gap; 16:00 never falls in
    // one for America/New_York, whose transitions happen at 02:00.
    let local = New_York
        .from_local_datetime(&naive)
        .single()
        .with_context(|| format!("16:00 ET on {expiry} is ambiguous or nonexistent"))?;
    Ok(local.with_timezone(&Utc))
}

/// Calendar year fraction from `now` to `expiry`, floored at zero.
///
/// Calendar, not trading, time — matching the reference implementation in
/// `~/ib-option-viz`. Its `CLAUDE.md` records that a trading-time `dt` was tried
/// for event scoring and ranked badly; for pricing, calendar time is what the
/// quoted vols were fitted against in the first place.
pub fn year_fraction(now: DateTime<Utc>, expiry: DateTime<Utc>) -> f64 {
    let seconds = (expiry - now).num_milliseconds() as f64 / 1000.0;
    (seconds / YEAR_SECONDS).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn expiry_resolves_to_2000_utc_during_daylight_time() {
        let got = expiry_instant(NaiveDate::from_ymd_opt(2026, 8, 21).unwrap()).unwrap();
        assert_eq!(got, Utc.with_ymd_and_hms(2026, 8, 21, 20, 0, 0).unwrap());
    }

    /// The reason this goes through a timezone database at all.
    #[test]
    fn expiry_resolves_to_2100_utc_during_standard_time() {
        let got = expiry_instant(NaiveDate::from_ymd_opt(2026, 12, 18).unwrap()).unwrap();
        assert_eq!(got, Utc.with_ymd_and_hms(2026, 12, 18, 21, 0, 0).unwrap());
    }

    #[test]
    fn year_fraction_counts_calendar_time() {
        let now = Utc.with_ymd_and_hms(2026, 8, 21, 20, 0, 0).unwrap();
        let year_later = Utc.with_ymd_and_hms(2027, 8, 21, 20, 0, 0).unwrap();
        assert!((year_fraction(now, year_later) - 365.0 / 365.25).abs() < 1e-9);
    }

    #[test]
    fn year_fraction_floors_at_zero_after_expiry() {
        let expiry = Utc.with_ymd_and_hms(2026, 8, 21, 20, 0, 0).unwrap();
        let later = Utc.with_ymd_and_hms(2026, 8, 25, 20, 0, 0).unwrap();
        assert_eq!(year_fraction(later, expiry), 0.0);
    }

    /// The realistic case the pricing tests use: 2026-08-19 15:00 UTC into the
    /// 2026-08-21 expiry is a touch under two days.
    #[test]
    fn a_late_chain_has_a_small_but_nonzero_year_fraction() {
        let now = Utc.with_ymd_and_hms(2026, 8, 19, 15, 0, 0).unwrap();
        let expiry = expiry_instant(NaiveDate::from_ymd_opt(2026, 8, 21).unwrap()).unwrap();
        let t = year_fraction(now, expiry);
        assert!((t - 2.2083333 / 365.25).abs() < 1e-6, "{t}");
    }
}
