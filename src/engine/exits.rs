//! When to close.
//!
//! All active rules are checked on every bar, OR'd — first to fire wins. The
//! priority within a single bar is: profit target, forecast target, underlying
//! past short strike (debit only), stop loss, forecast horizon, time decay,
//! before-earnings, before-expiry. On a bar that satisfies more than one, a
//! winner should not be logged as a stop-out.
//!
//! P&L rules read **gross** — exit credit minus entry debit, before commission.
//! A profit target is a resting limit order at a spread *price*, and no order
//! can be placed that accounts for its own commission.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::data::EarningsEvent;
use crate::domain::OptionRight;
use crate::model::PriceDistribution;
use crate::portfolio::{ExitReason, OpenPosition};
use crate::spread::{Side, Spread};

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct ExitRules {
    /// Favorable quantile used to materialize an entry's forecast price target.
    /// It belongs to the exit bundle even though the target is fixed at entry.
    pub forecast_target_quantile: Option<f64>,
    /// The mirror of the target: close when the underlying reaches this quantile
    /// of the entry forecast *against* the position — the bounds the trade was
    /// underwritten inside, breached.
    ///
    /// This is what "truly gone bad" means for a credit spread, and why it is not
    /// the live-POP floor. The floor reads a *probability*, so it fires on a
    /// position that is merely behind and still has the time it needs; this fires
    /// on a *price*, and only once the underlying has left the range the forecast
    /// gave it. Expressed like `forecast_target_quantile` — 0.85 means the 85th
    /// percentile of the move against the trade.
    pub forecast_stop_quantile: Option<f64>,
    /// Close once gross P&L reaches this percentage of max profit.
    pub profit_target_pct: Option<f64>,
    /// Close once gross P&L falls to minus this percentage of the debit paid.
    pub stop_loss_pct: Option<f64>,
    /// Close when the underlying crosses the short strike — near max profit.
    /// Applies to **debit** structures only: for a credit spread the short strike
    /// is a loss boundary, not a take-profit, and the underlying can already be
    /// past it at entry (a directional credit bet), which would open and instantly
    /// close the trade. Credit spreads keep the position for the full forecast
    /// window instead.
    pub underlying_past_short_strike: bool,
    /// Close once the position has been open at least this many days — the length
    /// of the forecast horizon it was scored to. `None` disables it. This is the
    /// time cap that closes a credit spread at the end of its forecast window and
    /// keeps any position from drifting past the model's view.
    pub forecast_horizon_days: Option<f64>,
    /// Spread give-up: close when the live probability of profit — the spread
    /// re-scored against the *current* forecast distribution — falls below this.
    /// Ranked mode only (needs the model at exit time).
    ///
    /// Applies to **any vertical, debit or credit**, and to no naked long. The
    /// question it asks — will this position clear what it cost? — is the same
    /// one either way: a debit clears by rising to more than it was bought for, a
    /// credit by staying cheaper to close than the premium taken in. It is the
    /// only loss-triggered exit a credit spread has, since the stop and the
    /// short-strike rule both require `entry_debit > 0`. Naked longs are excluded
    /// because they have their own give-ups (trailing stop, momentum), which let
    /// a winner run rather than reading a probability.
    pub live_pop_floor: Option<f64>,
    /// Naked-long trailing stop: close when gross P&L gives back this percentage
    /// of its running peak. Lets a winner run while capping the giveback.
    pub naked_trailing_stop_pct: Option<f64>,
    /// Naked-long momentum exit: close when the trend the entry leaned on flips
    /// against it. Ranked mode only (reads the current trend feature).
    pub naked_momentum_exit: bool,
    /// Close after this percentage of the time from entry to expiry has elapsed.
    pub max_time_pct: Option<f64>,
    /// Close at least this many calendar days before expiration.
    pub min_days_before_expiry: Option<u32>,
    /// Close before earnings announcements. The position is closed on the last
    /// session before the earnings event would affect prices.
    pub close_before_earnings: bool,
    /// The opposite policy: hold through the release, then close on the first
    /// bar after it is priced. For a vol-timing position the release *is* the
    /// trade, and once it has happened the reason to hold is gone.
    pub close_after_earnings: bool,
    /// Watch the price-level exits (`price_triggered`) on every stock-only bar
    /// between option bars — the 5-minute tape — and queue the close for the
    /// position's next option bar, so a stop crossed just after a chain snapshot
    /// is not left until the one after. Shared-account ranked runs only.
    pub stock_bar_exits: bool,
    /// Close once the position has been open this many days — a fixed holding
    /// period, for a rule-based strategy with no forecast horizon to key off.
    pub max_hold_days: Option<f64>,
}

/// The current forecast, handed to the exit check so a model-driven exit (live
/// POP, momentum) can read it. `None` on the staged path and when quotes are
/// unavailable. Its lifetime is the exit call — it borrows the bar's `ctx`.
pub struct ExitModel<'a> {
    /// The distribution of the underlying at the horizon, from this bar.
    pub dist: &'a PriceDistribution,
    /// Years from the horizon to expiry (for marking a spread at the horizon).
    pub remaining_years: f64,
    /// Sampled-spot -> forward scale, preserving the parity basis.
    pub forward_scale: f64,
    /// The current trend feature (fast/slow gap), for the momentum exit.
    pub trend: Option<f64>,
}

/// Everything the exit check needs beyond the position itself.
pub struct ExitContext<'a> {
    pub gross_pnl: f64,
    pub underlying_price: Option<f64>,
    pub bar_ts: DateTime<Utc>,
    pub expiry_instant: DateTime<Utc>,
    /// The next earnings event for this symbol, if any.
    pub next_earnings: Option<&'a EarningsEvent>,
    /// The latest release this bar is past (its last clean session was an
    /// earlier day), if any.
    pub last_release: Option<&'a EarningsEvent>,
    /// The current forecast, for model-driven exits. `None` on the staged path.
    pub model: Option<ExitModel<'a>>,
}

impl ExitRules {
    /// The gross P&L at which the trailing stop would close this position, or
    /// `None` when the rule cannot apply — not configured, not a naked long, or
    /// no peak established yet (a position that has never been in profit has
    /// nothing to trail).
    ///
    /// `triggered` compares against this rather than recomputing it, so the level
    /// a chart draws is the level the engine acts on.
    pub fn trailing_stop_level(&self, position: &OpenPosition) -> Option<f64> {
        let pct = self.naked_trailing_stop_pct?;
        let peak = position.peak_value?;
        (is_naked_long(position) && peak > 0.0).then(|| peak * (1.0 - pct / 100.0))
    }

    pub fn triggered(&self, position: &OpenPosition, ctx: &ExitContext<'_>) -> Option<ExitReason> {
        // 1. Profit target — best outcome.
        if let Some(pct) = self.profit_target_pct
            && let Some(max_profit) = position.max_profit
            && max_profit > 0.0
            && ctx.gross_pnl >= max_profit * pct / 100.0
        {
            return Some(ExitReason::ProfitTarget);
        }

        // 1b. The release the position was held for has happened.
        if self.close_after_earnings && past_a_held_release(position.opened_at, ctx.bar_ts, ctx.last_release) {
            return Some(ExitReason::AfterEarnings);
        }

        // 2a. The adverse bound: the underlying has left the range the entry
        //     forecast gave it, so the thesis the trade was written on is gone.
        //     Ahead of the profit-side rules below it because a breach is not a
        //     state to keep trading through.
        if let Some(stop) = position.forecast_stop
            && let Some(spot) = ctx.underlying_price
            && forecast_stop_reached(&position.spread, spot, stop)
        {
            return Some(ExitReason::ForecastStop);
        }

        // 2. Forecast target reached — the move the trade bet on has played out.
        if let Some(target) = position.forecast_target
            && let Some(spot) = ctx.underlying_price
            && forecast_target_reached(&position.spread, spot, target)
        {
            return Some(ExitReason::ForecastTarget);
        }

        // 3. Underlying past short strike — near max profit, take it. Debit
        //    structures only: for a credit spread this is a loss, not a profit,
        //    and can already be true at entry.
        if self.underlying_past_short_strike
            && position.entry_debit > 0.0
            && let Some(spot) = ctx.underlying_price
            && is_past_short_strike(&position.spread, spot)
        {
            return Some(ExitReason::UnderlyingPastShortStrike);
        }

        // 4. Stop loss.
        if let Some(pct) = self.stop_loss_pct
            && position.entry_debit > 0.0
            && ctx.gross_pnl <= -position.entry_debit * pct / 100.0
        {
            return Some(ExitReason::StopLoss);
        }

        // 5. Structure-scoped give-ups. Any vertical: give up when the live
        //    probability of profit falls below the floor. This is the credit
        //    side's only loss-triggered exit — everything above it is gated on
        //    `entry_debit > 0`.
        if let Some(floor) = self.live_pop_floor
            && is_vertical(position)
            && let Some(model) = &ctx.model
            && live_pop(position, model) < floor
        {
            return Some(ExitReason::PopFloor);
        }

        // Naked long: trailing stop — give back too much of the peak gain.
        if let Some(level) = self.trailing_stop_level(position)
            && ctx.gross_pnl <= level
        {
            return Some(ExitReason::TrailingStop);
        }

        // Naked long: momentum turned against the position.
        if self.naked_momentum_exit
            && is_naked_long(position)
            && !position.spread.is_direction_neutral()
            && let Some(model) = &ctx.model
            && let Some(trend) = model.trend
            && momentum_against(&position.spread, trend)
        {
            return Some(ExitReason::MomentumExit);
        }

        // 6. Forecast horizon elapsed — the forecast window has closed, so the
        //    model no longer speaks to the position. Closes credit spreads at the
        //    end of their full forecast duration.
        if let Some(reason) = forecast_horizon_exit(self, position.opened_at, ctx.bar_ts) {
            return Some(reason);
        }
        if held_for(self, position.opened_at, ctx.bar_ts) {
            return Some(ExitReason::HoldPeriod);
        }

        // 7. Time decay — too much of the life has been consumed.
        if let Some(max_pct) = self.max_time_pct {
            let total = (ctx.expiry_instant - position.opened_at).num_seconds() as f64;
            let elapsed = (ctx.bar_ts - position.opened_at).num_seconds() as f64;
            if total > 0.0 && elapsed / total * 100.0 >= max_pct {
                return Some(ExitReason::TimeDecay);
            }
        }

        // 8. Before-earnings — close before the announcement moves prices.
        if self.close_before_earnings
            && let Some(reason) = earnings_exit(ctx.bar_ts, ctx.next_earnings)
        {
            return Some(reason);
        }

        // 9. Before-expiry hard cutoff — avoid exercise/assignment risk.
        if let Some(days) = self.min_days_before_expiry {
            let remaining = (ctx.expiry_instant - ctx.bar_ts).num_days();
            if remaining < days as i64 {
                return Some(ExitReason::BeforeExpiry);
            }
        }

        None
    }

    /// The exits that read only the underlying's price against levels fixed at
    /// entry — the adverse-forecast stop, the forecast target, the short strike —
    /// in `triggered`'s order. They need no option quote and no fresh forecast,
    /// so `stock_bar_exits` checks them on every stock bar between option bars.
    pub fn price_triggered(&self, position: &OpenPosition, spot: f64) -> Option<ExitReason> {
        if let Some(stop) = position.forecast_stop
            && forecast_stop_reached(&position.spread, spot, stop)
        {
            return Some(ExitReason::ForecastStop);
        }
        if let Some(target) = position.forecast_target
            && forecast_target_reached(&position.spread, spot, target)
        {
            return Some(ExitReason::ForecastTarget);
        }
        self.underlying_price_triggered(position, spot)
    }

    /// Exit conditions that need only the underlying price, not option quotes.
    /// Called when `exit_credit` fails — the position's legs stopped quoting —
    /// but the underlying still prints. Higher priority than time-based rules,
    /// consistent with `triggered()`.
    pub fn underlying_price_triggered(
        &self,
        position: &OpenPosition,
        spot: f64,
    ) -> Option<ExitReason> {
        // Debit structures only — same rule as `triggered`.
        if self.underlying_past_short_strike
            && position.entry_debit > 0.0
            && is_past_short_strike(&position.spread, spot)
        {
            return Some(ExitReason::UnderlyingPastShortStrike);
        }
        None
    }

    /// Time-based exit conditions only. Called when option quotes are unavailable
    /// but the calendar keeps ticking — the position can be priced at intrinsic
    /// but not at market. Profit target and stop loss are skipped because
    /// intrinsic strips all time value; `underlying_past_short_strike` is handled
    /// separately by `underlying_price_triggered`.
    pub fn time_triggered(
        &self,
        opened_at: DateTime<Utc>,
        bar_ts: DateTime<Utc>,
        expiry_instant: DateTime<Utc>,
        next_earnings: Option<&EarningsEvent>,
        last_release: Option<&EarningsEvent>,
    ) -> Option<ExitReason> {
        if self.close_after_earnings && past_a_held_release(opened_at, bar_ts, last_release) {
            return Some(ExitReason::AfterEarnings);
        }
        if let Some(reason) = forecast_horizon_exit(self, opened_at, bar_ts) {
            return Some(reason);
        }
        if held_for(self, opened_at, bar_ts) {
            return Some(ExitReason::HoldPeriod);
        }

        if let Some(max_pct) = self.max_time_pct {
            let total = (expiry_instant - opened_at).num_seconds() as f64;
            let elapsed = (bar_ts - opened_at).num_seconds() as f64;
            if total > 0.0 && elapsed / total * 100.0 >= max_pct {
                return Some(ExitReason::TimeDecay);
            }
        }

        if self.close_before_earnings
            && let Some(reason) = earnings_exit(bar_ts, next_earnings)
        {
            return Some(reason);
        }

        if let Some(days) = self.min_days_before_expiry {
            let remaining = (expiry_instant - bar_ts).num_days();
            if remaining < days as i64 {
                return Some(ExitReason::BeforeExpiry);
            }
        }

        None
    }
}

/// Whether the position has been open its full `max_hold_days`.
fn held_for(rules: &ExitRules, opened_at: DateTime<Utc>, bar_ts: DateTime<Utc>) -> bool {
    rules
        .max_hold_days
        .is_some_and(|days| (bar_ts - opened_at).num_seconds() as f64 / 86_400.0 >= days)
}

/// Whether a position opened at `opened_at` was held across `last` — opened on
/// or before its last clean session — and `bar_ts` is past it.
fn past_a_held_release(
    opened_at: DateTime<Utc>,
    bar_ts: DateTime<Utc>,
    last: Option<&EarningsEvent>,
) -> bool {
    last.is_some_and(|e| opened_at.date_naive() <= e.close_on && bar_ts.date_naive() > e.close_on)
}

/// Close once `bar_ts` is at least `forecast_horizon_days` after `opened_at` —
/// the forecast window the position was scored to has elapsed.
fn forecast_horizon_exit(
    rules: &ExitRules,
    opened_at: DateTime<Utc>,
    bar_ts: DateTime<Utc>,
) -> Option<ExitReason> {
    let days = rules.forecast_horizon_days?;
    let elapsed_days = (bar_ts - opened_at).num_seconds() as f64 / 86_400.0;
    (elapsed_days >= days).then_some(ExitReason::ForecastHorizon)
}

/// Close on the last session before the release is priced in: the day of an
/// after-close release, the session before a pre-open one — a Friday for a
/// Monday morning, which "the calendar day before" would have made a Sunday and
/// never fired. A during-session release also closes the session before, since
/// by the day itself the print may already be out.
fn earnings_exit(bar_ts: DateTime<Utc>, next: Option<&EarningsEvent>) -> Option<ExitReason> {
    (bar_ts.date_naive() == next?.close_on).then_some(ExitReason::BeforeEarnings)
}

/// True if the bar falls within the earnings blackout window.
pub fn in_earnings_blackout(
    bar_ts: DateTime<Utc>,
    next_earnings: Option<&EarningsEvent>,
    blackout_days: u32,
) -> bool {
    let Some(event) = next_earnings else {
        return false;
    };
    let bar_date = bar_ts.date_naive();
    let days_until = (event.date - bar_date).num_days();
    days_until >= 0 && days_until < blackout_days as i64
}

/// True on the first `sessions` weekdays after a release is priced — the
/// reaction session and the days of drift after it. `last_release` is the
/// latest release whose last clean session (`close_on`) is before the bar.
// ponytail: counts weekdays, not sessions, so a holiday in the window shortens
// it by one; read the session tape if that ever matters.
pub fn in_post_earnings_blackout(
    bar_ts: DateTime<Utc>,
    last_release: Option<&EarningsEvent>,
    sessions: u32,
) -> bool {
    use chrono::Datelike;
    let Some(event) = last_release else {
        return false;
    };
    let bar_date = bar_ts.date_naive();
    let weekdays = event
        .close_on
        .iter_days()
        .skip(1)
        .take_while(|d| *d <= bar_date)
        .filter(|d| d.weekday().number_from_monday() <= 5)
        .count();
    weekdays >= 1 && weekdays <= sessions as usize
}

/// True when the underlying has crossed the short strike in the direction that
/// means the spread is near max profit.
/// Whether the underlying has reached the forecast target in the trade's favour.
fn forecast_target_reached(spread: &Spread, spot: f64, target: f64) -> bool {
    if spread.profits_from_a_rise() {
        spot >= target
    } else {
        spot <= target
    }
}

/// The adverse mirror: the underlying has moved *against* the position past the
/// bound. A structure that profits from a rise is stopped by a fall, and vice
/// versa — the opposite comparison to `forecast_target_reached`.
fn forecast_stop_reached(spread: &Spread, spot: f64, stop: f64) -> bool {
    if spread.profits_from_a_rise() {
        spot <= stop
    } else {
        spot >= stop
    }
}

/// A naked long — a single-leg long, no short leg to define a spread.
fn is_naked_long(position: &OpenPosition) -> bool {
    !position.spread.legs.iter().any(|l| l.side == Side::Short)
}

/// A vertical — anything carrying a short leg, which is what bounds its risk and
/// makes a probability of profit meaningful. Either orientation: the sign of
/// `entry_debit` says debit or credit, and the live-POP give-up asks the same
/// question of both.
fn is_vertical(position: &OpenPosition) -> bool {
    !is_naked_long(position)
}

/// The live probability the position clears its round-trip cost, re-scoring the
/// spread against the current forecast with its entry (sticky-strike) vols.
/// Works unchanged for a credit spread: `entry_debit` is negative there, so the
/// breakeven is the (negative) value at which closing costs exactly the credit
/// received, and `model_value` is likewise negative for a position that must be
/// bought back. The comparison is `value > cost basis` in both orientations.
fn live_pop(position: &OpenPosition, model: &ExitModel<'_>) -> f64 {
    let breakeven = position.entry_debit + 2.0 * position.entry_commission;
    crate::model::prob_profit_of(
        &position.spread,
        breakeven,
        &position.leg_vols,
        model.dist,
        model.remaining_years,
        model.forward_scale,
    )
}

/// True when the trend has flipped against the naked long's direction: a long
/// that profits from a rise wants a positive trend, and vice versa.
fn momentum_against(spread: &Spread, trend: f64) -> bool {
    if spread.profits_from_a_rise() {
        trend < 0.0
    } else {
        trend > 0.0
    }
}

fn is_past_short_strike(spread: &crate::spread::Spread, spot: f64) -> bool {
    for leg in &spread.legs {
        if leg.side != Side::Short {
            continue;
        }
        return match leg.right {
            OptionRight::Call => spot > leg.strike,
            OptionRight::Put => spot < leg.strike,
        };
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::OptionRight;
    use crate::spread::{Leg, Side, Spread};
    use chrono::{TimeZone, Utc};

    /// A $5-wide bull call bought for $170: max profit $330, max loss $170.
    fn position() -> OpenPosition {
        let spread = Spread::new(
            "SPY",
            vec![
                Leg {
                    contract_id: 1,
                    strike: 750.0,
                    right: OptionRight::Call,
                    side: Side::Long,
                    quantity: 1,
                },
                Leg {
                    contract_id: 2,
                    strike: 755.0,
                    right: OptionRight::Call,
                    side: Side::Short,
                    quantity: 1,
                },
            ],
        );
        OpenPosition {
            id: 1,
            expiry: chrono::NaiveDate::from_ymd_opt(2026, 8, 21).unwrap(),
            opened_at: Utc.with_ymd_and_hms(2026, 7, 1, 13, 30, 0).unwrap(),
            entry_debit: 170.0,
            entry_commission: 1.30,
            max_profit: Some(330.0),
            max_loss: Some(170.0),
            spread,
            prediction: None,
            forecast_target: None,
            forecast_stop: None,
            entry_features: None,
            leg_vols: Vec::new(),
            peak_value: None,
            estimated_entry: false,
        }
    }

    fn expiry_instant() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 21, 20, 0, 0).unwrap()
    }

    fn ctx(gross: f64, underlying: f64) -> ExitContext<'static> {
        ExitContext {
            gross_pnl: gross,
            underlying_price: Some(underlying),
            bar_ts: Utc.with_ymd_and_hms(2026, 7, 15, 15, 0, 0).unwrap(),
            expiry_instant: expiry_instant(),
            next_earnings: None,
            last_release: None,
            model: None,
        }
    }

    fn pnl_rules() -> ExitRules {
        ExitRules {
            profit_target_pct: Some(50.0),
            stop_loss_pct: Some(100.0),
            ..Default::default()
        }
    }

    #[test]
    fn the_profit_target_fires_at_half_of_max_profit() {
        assert_eq!(pnl_rules().triggered(&position(), &ctx(164.0, 751.0)), None);
        assert_eq!(
            pnl_rules().triggered(&position(), &ctx(165.0, 751.0)),
            Some(ExitReason::ProfitTarget)
        );
    }

    #[test]
    fn a_full_stop_fires_when_the_spread_is_worthless() {
        assert_eq!(
            pnl_rules().triggered(&position(), &ctx(-169.0, 751.0)),
            None
        );
        assert_eq!(
            pnl_rules().triggered(&position(), &ctx(-170.0, 751.0)),
            Some(ExitReason::StopLoss)
        );
    }

    #[test]
    fn a_half_stop_fires_at_half_the_debit() {
        let rules = ExitRules {
            stop_loss_pct: Some(50.0),
            ..Default::default()
        };
        assert_eq!(rules.triggered(&position(), &ctx(-84.0, 751.0)), None);
        assert_eq!(
            rules.triggered(&position(), &ctx(-85.0, 751.0)),
            Some(ExitReason::StopLoss)
        );
    }

    #[test]
    fn a_quiet_position_is_left_alone() {
        assert_eq!(pnl_rules().triggered(&position(), &ctx(0.0, 751.0)), None);
        assert_eq!(pnl_rules().triggered(&position(), &ctx(20.0, 751.0)), None);
    }

    #[test]
    fn the_target_reads_gross_pnl_not_net() {
        let p = position();
        let gross = 165.0;
        assert_eq!(
            pnl_rules().triggered(&p, &ctx(gross, 751.0)),
            Some(ExitReason::ProfitTarget)
        );

        let net = gross - p.entry_commission - 1.30;
        assert!(net < 165.0);
        assert_eq!(pnl_rules().triggered(&p, &ctx(net, 751.0)), None);
    }

    #[test]
    fn an_unbounded_position_has_no_profit_target_but_keeps_its_stop() {
        let mut p = position();
        p.max_profit = None;
        assert_eq!(
            pnl_rules().triggered(&p, &ctx(100_000.0, 751.0)),
            None
        );
        assert_eq!(
            pnl_rules().triggered(&p, &ctx(-200.0, 751.0)),
            Some(ExitReason::StopLoss)
        );
    }

    #[test]
    fn no_rules_means_no_exits() {
        let none = ExitRules::default();
        assert_eq!(none.triggered(&position(), &ctx(10_000.0, 751.0)), None);
        assert_eq!(none.triggered(&position(), &ctx(-10_000.0, 751.0)), None);
    }

    #[test]
    fn the_profit_target_wins_a_tie() {
        let both = ExitRules {
            profit_target_pct: Some(1.0),
            stop_loss_pct: Some(0.0),
            ..Default::default()
        };
        assert_eq!(
            both.triggered(&position(), &ctx(5.0, 751.0)),
            Some(ExitReason::ProfitTarget)
        );
    }

    // ---- new exit conditions ----

    #[test]
    fn underlying_past_short_strike_fires_for_bull_call() {
        let rules = ExitRules {
            underlying_past_short_strike: true,
            ..Default::default()
        };
        // Short strike is 755. At 754.99, not triggered.
        assert_eq!(rules.triggered(&position(), &ctx(0.0, 754.99)), None);
        // At 755.01, triggered.
        assert_eq!(
            rules.triggered(&position(), &ctx(0.0, 755.01)),
            Some(ExitReason::UnderlyingPastShortStrike)
        );
    }

    #[test]
    fn forecast_target_fires_in_the_trade_direction() {
        // A bull call profits from a rise, so its target is an upper level.
        let mut bull = position();
        bull.forecast_target = Some(760.0);
        let rules = ExitRules::default();
        assert_eq!(rules.triggered(&bull, &ctx(0.0, 759.99)), None);
        assert_eq!(
            rules.triggered(&bull, &ctx(0.0, 760.01)),
            Some(ExitReason::ForecastTarget)
        );

        // A bear put profits from a fall — the target is a lower level.
        let mut bear = position();
        bear.spread = Spread::new(
            "SPY",
            vec![
                crate::spread::Leg {
                    contract_id: 1,
                    strike: 755.0,
                    right: OptionRight::Put,
                    side: Side::Long,
                    quantity: 1,
                },
                crate::spread::Leg {
                    contract_id: 2,
                    strike: 745.0,
                    right: OptionRight::Put,
                    side: Side::Short,
                    quantity: 1,
                },
            ],
        );
        bear.forecast_target = Some(748.0);
        assert_eq!(rules.triggered(&bear, &ctx(0.0, 748.01)), None);
        assert_eq!(
            rules.triggered(&bear, &ctx(0.0, 747.99)),
            Some(ExitReason::ForecastTarget)
        );
    }

    #[test]
    fn underlying_past_short_strike_fires_for_bear_put() {
        let spread = Spread::new(
            "SPY",
            vec![
                Leg {
                    contract_id: 1,
                    strike: 750.0,
                    right: OptionRight::Put,
                    side: Side::Long,
                    quantity: 1,
                },
                Leg {
                    contract_id: 2,
                    strike: 745.0,
                    right: OptionRight::Put,
                    side: Side::Short,
                    quantity: 1,
                },
            ],
        );
        let p = OpenPosition {
            id: 1,
            expiry: chrono::NaiveDate::from_ymd_opt(2026, 8, 21).unwrap(),
            opened_at: Utc.with_ymd_and_hms(2026, 7, 1, 13, 30, 0).unwrap(),
            entry_debit: 170.0,
            entry_commission: 1.30,
            max_profit: Some(330.0),
            max_loss: Some(170.0),
            spread,
            prediction: None,
            forecast_target: None,
            forecast_stop: None,
            entry_features: None,
            leg_vols: Vec::new(),
            peak_value: None,
            estimated_entry: false,
        };
        let rules = ExitRules {
            underlying_past_short_strike: true,
            ..Default::default()
        };
        // Short strike is 745. At 745.01, not triggered (put needs spot < strike).
        assert_eq!(rules.triggered(&p, &ctx(0.0, 745.01)), None);
        // At 744.99, triggered.
        assert_eq!(
            rules.triggered(&p, &ctx(0.0, 744.99)),
            Some(ExitReason::UnderlyingPastShortStrike)
        );
    }

    /// The bug this fixes: a credit spread whose short strike the underlying is
    /// already past (a directional credit bet) must NOT close on the short-strike
    /// exit — for a credit that strike is a loss, not a take-profit.
    #[test]
    fn a_credit_spread_never_closes_on_the_short_strike() {
        // A bear call credit spread: short 750 call, long 755 call, taken for a
        // $1.30 credit. The short strike (750) is below spot from the start.
        let spread = Spread::new(
            "SPY",
            vec![
                Leg {
                    contract_id: 1,
                    strike: 750.0,
                    right: OptionRight::Call,
                    side: Side::Short,
                    quantity: 1,
                },
                Leg {
                    contract_id: 2,
                    strike: 755.0,
                    right: OptionRight::Call,
                    side: Side::Long,
                    quantity: 1,
                },
            ],
        );
        let credit = OpenPosition {
            id: 1,
            expiry: chrono::NaiveDate::from_ymd_opt(2026, 8, 21).unwrap(),
            opened_at: Utc.with_ymd_and_hms(2026, 7, 1, 13, 30, 0).unwrap(),
            entry_debit: -130.0, // negative: a credit was received
            entry_commission: 1.30,
            max_profit: Some(130.0),
            max_loss: Some(370.0),
            spread,
            prediction: None,
            forecast_target: None,
            forecast_stop: None,
            entry_features: None,
            leg_vols: Vec::new(),
            peak_value: None,
            estimated_entry: false,
        };
        let rules = ExitRules {
            underlying_past_short_strike: true,
            ..Default::default()
        };
        // Spot 760 is well past the short strike, yet nothing fires — a credit
        // spread is held, not closed, on the short strike.
        assert_eq!(rules.triggered(&credit, &ctx(0.0, 760.0)), None);
        assert_eq!(rules.underlying_price_triggered(&credit, 760.0), None);
    }

    /// A single-leg long call at 750 — a naked long that profits from a rise.
    fn naked_call() -> OpenPosition {
        let mut p = position();
        p.spread = Spread::new(
            "SPY",
            vec![Leg {
                contract_id: 1,
                strike: 750.0,
                right: OptionRight::Call,
                side: Side::Long,
                quantity: 1,
            }],
        );
        p.max_profit = None; // unbounded
        p.leg_vols = vec![0.2];
        p
    }

    fn ctx_with_model<'a>(
        gross: f64,
        underlying: f64,
        model: ExitModel<'a>,
    ) -> ExitContext<'a> {
        ExitContext {
            model: Some(model),
            ..ctx(gross, underlying)
        }
    }

    #[test]
    fn pop_floor_gives_up_a_debit_spread_but_never_a_naked() {
        use crate::model::PriceDistribution;
        // Mass well below the 750/755 bull call, so the spread prices near zero
        // and its probability of clearing cost is ~0.
        let dist =
            PriceDistribution::from_weights(vec![700.0, 720.0, 740.0], vec![5.0, 3.0, 1.0]).unwrap();
        let model = ExitModel {
            dist: &dist,
            remaining_years: 0.02,
            forward_scale: 1.0,
            trend: None,
        };
        let rules = ExitRules {
            live_pop_floor: Some(0.5),
            ..Default::default()
        };
        let mut spread = position(); // debit vertical
        spread.leg_vols = vec![0.2, 0.2];
        assert_eq!(
            rules.triggered(&spread, &ctx_with_model(-50.0, 720.0, model)),
            Some(ExitReason::PopFloor)
        );
        // Same rule, a naked long: the POP floor is scoped to verticals only,
        // because a naked long has its own give-ups.
        let model2 = ExitModel {
            dist: &dist,
            remaining_years: 0.02,
            forward_scale: 1.0,
            trend: None,
        };
        assert_eq!(
            rules.triggered(&naked_call(), &ctx_with_model(-50.0, 720.0, model2)),
            None
        );
    }

    /// The credit side's only loss-triggered exit. Everything above the POP floor
    /// in the priority order is gated on `entry_debit > 0`, so before this rule
    /// covered credit, a losing credit spread could only close on the calendar.
    #[test]
    fn pop_floor_gives_up_a_credit_spread_the_forecast_has_turned_against() {
        use crate::model::PriceDistribution;
        // A bear call credit spread, short 750 / long 755, taken for $1.30. Mass
        // well *above* both strikes: it will be bought back at close to the full
        // $5 width, so the probability of clearing the credit is ~0.
        let mut credit = position();
        credit.spread = Spread::new(
            "SPY",
            vec![
                Leg {
                    contract_id: 1,
                    strike: 750.0,
                    right: OptionRight::Call,
                    side: Side::Short,
                    quantity: 1,
                },
                Leg {
                    contract_id: 2,
                    strike: 755.0,
                    right: OptionRight::Call,
                    side: Side::Long,
                    quantity: 1,
                },
            ],
        );
        credit.entry_debit = -130.0; // negative: a credit was received
        credit.max_profit = Some(130.0);
        credit.max_loss = Some(370.0);
        credit.leg_vols = vec![0.2, 0.2];

        let against =
            PriceDistribution::from_weights(vec![780.0, 800.0, 820.0], vec![1.0, 3.0, 5.0]).unwrap();
        let rules = ExitRules {
            live_pop_floor: Some(0.5),
            ..Default::default()
        };
        assert_eq!(
            rules.triggered(
                &credit,
                &ctx_with_model(
                    -200.0,
                    800.0,
                    ExitModel {
                        dist: &against,
                        remaining_years: 0.02,
                        forward_scale: 1.0,
                        trend: None,
                    }
                )
            ),
            Some(ExitReason::PopFloor)
        );

        // And it holds while the forecast still says the short strike expires
        // worthless — the rule gives up on the forecast, not on a drawdown.
        let favourable =
            PriceDistribution::from_weights(vec![700.0, 710.0, 720.0], vec![5.0, 3.0, 1.0]).unwrap();
        assert_eq!(
            rules.triggered(
                &credit,
                &ctx_with_model(
                    -200.0,
                    710.0,
                    ExitModel {
                        dist: &favourable,
                        remaining_years: 0.02,
                        forward_scale: 1.0,
                        trend: None,
                    }
                )
            ),
            None
        );
    }

    /// "Truly gone bad" is a price statement, not a probability one: the
    /// underlying has left the range the entry forecast gave it. That is why this
    /// is not the live-POP floor, which fires on a position merely behind.
    #[test]
    fn the_forecast_stop_fires_only_once_the_underlying_leaves_its_entry_bounds() {
        let rules = ExitRules {
            forecast_stop_quantile: Some(0.85),
            ..Default::default()
        };

        // A bull call (profits from a rise) stopped by a *fall* through 735.
        let mut bull = position();
        bull.forecast_stop = Some(735.0);
        assert_eq!(rules.triggered(&bull, &ctx(-50.0, 734.0)), Some(ExitReason::ForecastStop));
        assert_eq!(rules.triggered(&bull, &ctx(-50.0, 736.0)), None, "still inside the bounds");

        // A bear put (profits from a fall) is stopped by a *rise* — the mirror.
        let mut bear = position();
        bear.spread = Spread::new(
            "SPY",
            vec![
                Leg { contract_id: 1, strike: 760.0, right: OptionRight::Put, side: Side::Long, quantity: 1 },
                Leg { contract_id: 2, strike: 755.0, right: OptionRight::Put, side: Side::Short, quantity: 1 },
            ],
        );
        bear.forecast_stop = Some(775.0);
        assert_eq!(rules.triggered(&bear, &ctx(-50.0, 776.0)), Some(ExitReason::ForecastStop));
        assert_eq!(rules.triggered(&bear, &ctx(-50.0, 774.0)), None);

        // And a position with no bound set is never stopped by it.
        assert_eq!(rules.triggered(&position(), &ctx(-50.0, 100.0)), None);
    }

    #[test]
    fn trailing_stop_fires_after_a_naked_long_gives_back_its_peak() {
        let rules = ExitRules {
            naked_trailing_stop_pct: Some(30.0),
            ..Default::default()
        };
        let mut naked = naked_call();
        naked.peak_value = Some(100.0); // peaked at a $100 gain
        // Still holding most of the gain (above 70): no exit.
        assert_eq!(rules.triggered(&naked, &ctx(80.0, 760.0)), None);
        // Gave back past 30% of the peak: exit.
        assert_eq!(
            rules.triggered(&naked, &ctx(65.0, 760.0)),
            Some(ExitReason::TrailingStop)
        );
        // A debit spread (has a short leg) is not subject to the naked trailing stop.
        let mut spread = position();
        spread.peak_value = Some(100.0);
        assert_eq!(rules.triggered(&spread, &ctx(50.0, 760.0)), None);
    }

    #[test]
    fn momentum_exit_fires_when_the_trend_turns_against_a_naked_long() {
        use crate::model::PriceDistribution;
        let dist = PriceDistribution::from_weights(vec![760.0], vec![1.0]).unwrap();
        let rules = ExitRules {
            naked_momentum_exit: true,
            ..Default::default()
        };
        // Long call profits from a rise; a negative trend is against it -> exit.
        let against = ExitModel {
            dist: &dist,
            remaining_years: 0.02,
            forward_scale: 1.0,
            trend: Some(-0.01),
        };
        assert_eq!(
            rules.triggered(&naked_call(), &ctx_with_model(0.0, 760.0, against)),
            Some(ExitReason::MomentumExit)
        );
        // A positive trend is with the long call -> hold.
        let with = ExitModel {
            dist: &dist,
            remaining_years: 0.02,
            forward_scale: 1.0,
            trend: Some(0.01),
        };
        assert_eq!(
            rules.triggered(&naked_call(), &ctx_with_model(0.0, 760.0, with)),
            None
        );
    }

    #[test]
    fn the_forecast_horizon_closes_a_position_after_its_window() {
        let rules = ExitRules {
            forecast_horizon_days: Some(7.0),
            ..Default::default()
        };
        let p = position(); // opened Jul 1 13:30
        // Six days in: still inside the forecast window.
        let inside = ExitContext {
            gross_pnl: 0.0,
            underlying_price: Some(751.0),
            bar_ts: Utc.with_ymd_and_hms(2026, 7, 7, 13, 30, 0).unwrap(),
            expiry_instant: expiry_instant(),
            next_earnings: None,
            last_release: None,
            model: None,
        };
        assert_eq!(rules.triggered(&p, &inside), None);
        // Just past seven days: the forecast window has elapsed.
        let elapsed = ExitContext {
            bar_ts: Utc.with_ymd_and_hms(2026, 7, 8, 14, 0, 0).unwrap(),
            ..inside
        };
        assert_eq!(rules.triggered(&p, &elapsed), Some(ExitReason::ForecastHorizon));
        // It also fires on the quote-unavailable path.
        assert_eq!(
            rules.time_triggered(p.opened_at, elapsed.bar_ts, expiry_instant(), None, None),
            Some(ExitReason::ForecastHorizon)
        );
    }

    #[test]
    fn profit_target_beats_underlying_past_short_strike() {
        let rules = ExitRules {
            profit_target_pct: Some(50.0),
            underlying_past_short_strike: true,
            ..Default::default()
        };
        // Both conditions met: spot > 755, gross > 165.
        assert_eq!(
            rules.triggered(&position(), &ctx(200.0, 760.0)),
            Some(ExitReason::ProfitTarget)
        );
    }

    #[test]
    fn time_decay_fires_after_half_the_life() {
        let rules = ExitRules {
            max_time_pct: Some(50.0),
            ..Default::default()
        };
        let p = position();
        // Entry: Jul 1, expiry: Aug 21 = 51 days. 50% = ~25.5 days = Jul 26-ish.
        let before = ExitContext {
            gross_pnl: 0.0,
            underlying_price: Some(751.0),
            bar_ts: Utc.with_ymd_and_hms(2026, 7, 25, 15, 0, 0).unwrap(),
            expiry_instant: expiry_instant(),
            next_earnings: None,
            last_release: None,
            model: None,
        };
        assert_eq!(rules.triggered(&p, &before), None);

        let after = ExitContext {
            bar_ts: Utc.with_ymd_and_hms(2026, 7, 27, 15, 0, 0).unwrap(),
            ..before
        };
        assert_eq!(
            rules.triggered(&p, &after),
            Some(ExitReason::TimeDecay)
        );
    }

    #[test]
    fn before_expiry_fires_within_the_cutoff() {
        let rules = ExitRules {
            min_days_before_expiry: Some(7),
            ..Default::default()
        };
        // 8 days before expiry: safe.
        let safe = ExitContext {
            gross_pnl: 0.0,
            underlying_price: Some(751.0),
            bar_ts: Utc.with_ymd_and_hms(2026, 8, 13, 15, 0, 0).unwrap(),
            expiry_instant: expiry_instant(),
            next_earnings: None,
            last_release: None,
            model: None,
        };
        assert_eq!(rules.triggered(&position(), &safe), None);

        // 6 days before expiry: close it.
        let too_close = ExitContext {
            bar_ts: Utc.with_ymd_and_hms(2026, 8, 15, 20, 1, 0).unwrap(),
            ..safe
        };
        assert_eq!(
            rules.triggered(&position(), &too_close),
            Some(ExitReason::BeforeExpiry)
        );
    }

    #[test]
    fn all_rules_compose_and_first_match_wins() {
        let rules = ExitRules {
            underlying_past_short_strike: true,
            stop_loss_pct: Some(100.0),
            max_time_pct: Some(50.0),
            min_days_before_expiry: Some(7),
            ..Default::default()
        };
        // Early in the trade, spot past short strike -> that rule wins.
        assert_eq!(
            rules.triggered(&position(), &ctx(0.0, 760.0)),
            Some(ExitReason::UnderlyingPastShortStrike)
        );

        // Stop loss beats time decay.
        let late = ExitContext {
            gross_pnl: -170.0,
            underlying_price: Some(740.0),
            bar_ts: Utc.with_ymd_and_hms(2026, 8, 10, 15, 0, 0).unwrap(),
            expiry_instant: expiry_instant(),
            next_earnings: None,
            last_release: None,
            model: None,
        };
        assert_eq!(
            rules.triggered(&position(), &late),
            Some(ExitReason::StopLoss)
        );
    }

    // ---- time_triggered: fallback when quotes are unavailable ----

    #[test]
    fn time_triggered_fires_on_time_based_rules_only() {
        let rules = ExitRules {
            profit_target_pct: Some(50.0),
            stop_loss_pct: Some(100.0),
            underlying_past_short_strike: true,
            max_time_pct: Some(50.0),
            min_days_before_expiry: Some(7),
            ..Default::default()
        };
        let p = position();

        // Early: none of the time rules fire.
        assert_eq!(
            rules.time_triggered(
                p.opened_at,
                Utc.with_ymd_and_hms(2026, 7, 15, 15, 0, 0).unwrap(),
                expiry_instant(),
                None,
                None,
            ),
            None,
        );

        // Late enough for time decay (50% elapsed).
        assert_eq!(
            rules.time_triggered(
                p.opened_at,
                Utc.with_ymd_and_hms(2026, 7, 27, 15, 0, 0).unwrap(),
                expiry_instant(),
                None,
                None,
            ),
            Some(ExitReason::TimeDecay),
        );

        // Close to expiry: before-expiry wins (time_decay fires first in priority).
        assert_eq!(
            rules.time_triggered(
                p.opened_at,
                Utc.with_ymd_and_hms(2026, 8, 18, 15, 0, 0).unwrap(),
                expiry_instant(),
                None,
                None,
            ),
            Some(ExitReason::TimeDecay),
        );
    }

    #[test]
    fn time_triggered_ignores_price_based_rules() {
        let rules = ExitRules {
            profit_target_pct: Some(1.0),
            stop_loss_pct: Some(1.0),
            underlying_past_short_strike: true,
            ..Default::default()
        };
        // No time-based rules configured — should never fire.
        assert_eq!(
            rules.time_triggered(
                Utc.with_ymd_and_hms(2026, 7, 1, 13, 30, 0).unwrap(),
                Utc.with_ymd_and_hms(2026, 8, 20, 19, 30, 0).unwrap(),
                expiry_instant(),
                None,
                None,
            ),
            None,
        );
    }

    // ---- underlying_price_triggered ----

    #[test]
    fn underlying_price_triggered_fires_past_short_strike() {
        let rules = ExitRules {
            underlying_past_short_strike: true,
            ..Default::default()
        };
        let p = position(); // short call at 755
        assert_eq!(
            rules.underlying_price_triggered(&p, 754.99),
            None,
        );
        assert_eq!(
            rules.underlying_price_triggered(&p, 755.01),
            Some(ExitReason::UnderlyingPastShortStrike),
        );
    }

    #[test]
    fn underlying_price_triggered_is_silent_when_disabled() {
        let rules = ExitRules::default();
        let p = position();
        assert_eq!(
            rules.underlying_price_triggered(&p, 800.0),
            None,
        );
    }

    // ---- earnings exit ----

    #[test]
    fn close_before_after_close_earnings() {
        use crate::data::EarningsEvent;
        let event = EarningsEvent {
            date: chrono::NaiveDate::from_ymd_opt(2026, 7, 29).unwrap(),
            close_on: chrono::NaiveDate::from_ymd_opt(2026, 7, 29).unwrap(),
        };
        let rules = ExitRules {
            close_before_earnings: true,
            ..Default::default()
        };
        // Day of earnings (after_close) → close today.
        let ctx_today = ExitContext {
            gross_pnl: 0.0,
            underlying_price: Some(751.0),
            bar_ts: Utc.with_ymd_and_hms(2026, 7, 29, 15, 0, 0).unwrap(),
            expiry_instant: expiry_instant(),
            next_earnings: Some(&event),
            last_release: None,
            model: None,
        };
        assert_eq!(
            rules.triggered(&position(), &ctx_today),
            Some(ExitReason::BeforeEarnings)
        );

        // Day before → no action yet.
        let ctx_before = ExitContext {
            bar_ts: Utc.with_ymd_and_hms(2026, 7, 28, 15, 0, 0).unwrap(),
            ..ctx_today
        };
        assert_eq!(rules.triggered(&position(), &ctx_before), None);
    }

    #[test]
    fn close_before_before_open_earnings() {
        use crate::data::EarningsEvent;
        let event = EarningsEvent {
            date: chrono::NaiveDate::from_ymd_opt(2026, 7, 29).unwrap(),
            close_on: chrono::NaiveDate::from_ymd_opt(2026, 7, 28).unwrap(),
        };
        let rules = ExitRules {
            close_before_earnings: true,
            ..Default::default()
        };
        // Day before the before_open event → close today.
        let ctx = ExitContext {
            gross_pnl: 0.0,
            underlying_price: Some(751.0),
            bar_ts: Utc.with_ymd_and_hms(2026, 7, 28, 15, 0, 0).unwrap(),
            expiry_instant: expiry_instant(),
            next_earnings: Some(&event),
            last_release: None,
            model: None,
        };
        assert_eq!(
            rules.triggered(&position(), &ctx),
            Some(ExitReason::BeforeEarnings)
        );
    }

    /// Held across an after-close release on 7/29 (last clean session 7/29): the
    /// first bar of 7/30 closes it. A position opened after the release, or a
    /// bar still on 7/29, does not.
    #[test]
    fn close_after_earnings_fires_once_the_held_release_is_priced() {
        use crate::data::EarningsEvent;
        let event = EarningsEvent {
            date: chrono::NaiveDate::from_ymd_opt(2026, 7, 29).unwrap(),
            close_on: chrono::NaiveDate::from_ymd_opt(2026, 7, 29).unwrap(),
        };
        let rules = ExitRules {
            close_after_earnings: true,
            ..Default::default()
        };
        let ctx = |day, last| ExitContext {
            gross_pnl: 0.0,
            underlying_price: Some(751.0),
            bar_ts: Utc.with_ymd_and_hms(2026, 7, day, 13, 30, 0).unwrap(),
            expiry_instant: expiry_instant(),
            next_earnings: None,
            last_release: last,
            model: None,
        };
        let held = position(); // opened 7/1
        assert_eq!(rules.triggered(&held, &ctx(30, Some(&event))), Some(ExitReason::AfterEarnings));
        assert_eq!(rules.triggered(&held, &ctx(29, None)), None, "still before the print");

        let mut late = position();
        late.opened_at = Utc.with_ymd_and_hms(2026, 7, 30, 13, 30, 0).unwrap();
        assert_eq!(rules.triggered(&late, &ctx(31, Some(&event))), None, "never held it");
        assert_eq!(ExitRules::default().triggered(&held, &ctx(30, Some(&event))), None);
    }

    #[test]
    fn a_fixed_holding_period_closes_on_its_day() {
        let rules = ExitRules {
            max_hold_days: Some(21.0),
            ..Default::default()
        };
        let p = position(); // opened 7/1 13:30
        let at = |day, hour| Utc.with_ymd_and_hms(2026, 7, day, hour, 30, 0).unwrap();
        assert_eq!(rules.time_triggered(p.opened_at, at(22, 12), expiry_instant(), None, None), None);
        assert_eq!(
            rules.time_triggered(p.opened_at, at(22, 13), expiry_instant(), None, None),
            Some(ExitReason::HoldPeriod)
        );
    }

    #[test]
    fn earnings_exit_disabled_by_default() {
        use crate::data::EarningsEvent;
        let event = EarningsEvent {
            date: chrono::NaiveDate::from_ymd_opt(2026, 7, 29).unwrap(),
            close_on: chrono::NaiveDate::from_ymd_opt(2026, 7, 29).unwrap(),
        };
        let rules = ExitRules::default();
        let ctx = ExitContext {
            gross_pnl: 0.0,
            underlying_price: Some(751.0),
            bar_ts: Utc.with_ymd_and_hms(2026, 7, 29, 15, 0, 0).unwrap(),
            expiry_instant: expiry_instant(),
            next_earnings: Some(&event),
            last_release: None,
            model: None,
        };
        assert_eq!(rules.triggered(&position(), &ctx), None);
    }

    #[test]
    fn the_post_earnings_blackout_covers_the_sessions_after_the_release() {
        use crate::data::EarningsEvent;
        // A Thursday after-close release: Thursday is its last clean session.
        let event = EarningsEvent {
            date: chrono::NaiveDate::from_ymd_opt(2026, 7, 30).unwrap(),
            close_on: chrono::NaiveDate::from_ymd_opt(2026, 7, 30).unwrap(),
        };
        let on = |d| in_post_earnings_blackout(Utc.with_ymd_and_hms(2026, 7, d, 15, 0, 0).unwrap(), Some(&event), 5);
        let aug = |d| in_post_earnings_blackout(Utc.with_ymd_and_hms(2026, 8, d, 15, 0, 0).unwrap(), Some(&event), 5);
        assert!(!on(30), "the release day itself belongs to the pre-release gate");
        assert!(on(31), "the reaction session is session 1");
        // Mon 3 .. Thu 6 are sessions 2-5; the weekend counts for nothing.
        assert!(aug(3) && aug(6));
        assert!(!aug(7), "session 6 trades again");
        assert!(!in_post_earnings_blackout(Utc.with_ymd_and_hms(2026, 7, 31, 15, 0, 0).unwrap(), None, 5));
    }

    #[test]
    fn earnings_blackout_window() {
        use crate::data::EarningsEvent;
        let event = EarningsEvent {
            date: chrono::NaiveDate::from_ymd_opt(2026, 7, 29).unwrap(),
            close_on: chrono::NaiveDate::from_ymd_opt(2026, 7, 29).unwrap(),
        };
        // 3-day blackout: Jul 27, 28, 29 are blocked (days_until = 2, 1, 0).
        assert!(!in_earnings_blackout(
            Utc.with_ymd_and_hms(2026, 7, 25, 15, 0, 0).unwrap(),
            Some(&event),
            3,
        ));
        assert!(!in_earnings_blackout(
            Utc.with_ymd_and_hms(2026, 7, 26, 15, 0, 0).unwrap(),
            Some(&event),
            3,
        ));
        assert!(in_earnings_blackout(
            Utc.with_ymd_and_hms(2026, 7, 27, 15, 0, 0).unwrap(),
            Some(&event),
            3,
        ));
        assert!(in_earnings_blackout(
            Utc.with_ymd_and_hms(2026, 7, 28, 15, 0, 0).unwrap(),
            Some(&event),
            3,
        ));
        // Earnings day itself is also in the window.
        assert!(in_earnings_blackout(
            Utc.with_ymd_and_hms(2026, 7, 29, 15, 0, 0).unwrap(),
            Some(&event),
            3,
        ));
        // Day after earnings is not blocked.
        assert!(!in_earnings_blackout(
            Utc.with_ymd_and_hms(2026, 7, 30, 15, 0, 0).unwrap(),
            Some(&event),
            3,
        ));
        // No earnings → no blackout.
        assert!(!in_earnings_blackout(
            Utc.with_ymd_and_hms(2026, 7, 27, 15, 0, 0).unwrap(),
            None,
            3,
        ));
    }
}
