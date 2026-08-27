//! When to close.
//!
//! All active rules are checked on every bar, OR'd — first to fire wins. The
//! priority within a single bar is: profit target, underlying past short strike,
//! stop loss, time decay, before-expiry hard cutoff. On a bar that satisfies
//! more than one, a winner should not be logged as a stop-out.
//!
//! P&L rules read **gross** — exit credit minus entry debit, before commission.
//! A profit target is a resting limit order at a spread *price*, and no order
//! can be placed that accounts for its own commission.

use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::domain::OptionRight;
use crate::portfolio::{ExitReason, OpenPosition};
use crate::spread::Side;

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct ExitRules {
    /// Close once gross P&L reaches this percentage of max profit.
    pub profit_target_pct: Option<f64>,
    /// Close once gross P&L falls to minus this percentage of the debit paid.
    pub stop_loss_pct: Option<f64>,
    /// Close when the underlying crosses the short strike — near max profit.
    pub underlying_past_short_strike: bool,
    /// Close after this percentage of the time from entry to expiry has elapsed.
    pub max_time_pct: Option<f64>,
    /// Close at least this many calendar days before expiration.
    pub min_days_before_expiry: Option<u32>,
}

/// Everything the exit check needs beyond the position itself.
pub struct ExitContext {
    pub gross_pnl: f64,
    pub underlying_price: Option<f64>,
    pub bar_ts: DateTime<Utc>,
    pub expiry_instant: DateTime<Utc>,
}

impl ExitRules {
    pub fn triggered(&self, position: &OpenPosition, ctx: &ExitContext) -> Option<ExitReason> {
        // 1. Profit target — best outcome.
        if let Some(pct) = self.profit_target_pct
            && let Some(max_profit) = position.max_profit
            && max_profit > 0.0
            && ctx.gross_pnl >= max_profit * pct / 100.0
        {
            return Some(ExitReason::ProfitTarget);
        }

        // 2. Underlying past short strike — near max profit, take it.
        if self.underlying_past_short_strike
            && let Some(spot) = ctx.underlying_price
            && is_past_short_strike(&position.spread, spot)
        {
            return Some(ExitReason::UnderlyingPastShortStrike);
        }

        // 3. Stop loss.
        if let Some(pct) = self.stop_loss_pct
            && position.entry_debit > 0.0
            && ctx.gross_pnl <= -position.entry_debit * pct / 100.0
        {
            return Some(ExitReason::StopLoss);
        }

        // 4. Time decay — too much of the life has been consumed.
        if let Some(max_pct) = self.max_time_pct {
            let total = (ctx.expiry_instant - position.opened_at).num_seconds() as f64;
            let elapsed = (ctx.bar_ts - position.opened_at).num_seconds() as f64;
            if total > 0.0 && elapsed / total * 100.0 >= max_pct {
                return Some(ExitReason::TimeDecay);
            }
        }

        // 5. Before-expiry hard cutoff — avoid exercise/assignment risk.
        if let Some(days) = self.min_days_before_expiry {
            let remaining = (ctx.expiry_instant - ctx.bar_ts).num_days();
            if remaining < days as i64 {
                return Some(ExitReason::BeforeExpiry);
            }
        }

        None
    }
}

/// True when the underlying has crossed the short strike in the direction that
/// means the spread is near max profit.
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
            opened_at: Utc.with_ymd_and_hms(2026, 7, 1, 13, 30, 0).unwrap(),
            entry_debit: 170.0,
            entry_commission: 1.30,
            max_profit: Some(330.0),
            max_loss: Some(170.0),
            spread,
        }
    }

    fn expiry_instant() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 21, 20, 0, 0).unwrap()
    }

    fn ctx(gross: f64, underlying: f64) -> ExitContext {
        ExitContext {
            gross_pnl: gross,
            underlying_price: Some(underlying),
            bar_ts: Utc.with_ymd_and_hms(2026, 7, 15, 15, 0, 0).unwrap(),
            expiry_instant: expiry_instant(),
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
            opened_at: Utc.with_ymd_and_hms(2026, 7, 1, 13, 30, 0).unwrap(),
            entry_debit: 170.0,
            entry_commission: 1.30,
            max_profit: Some(330.0),
            max_loss: Some(170.0),
            spread,
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
        };
        assert_eq!(
            rules.triggered(&position(), &late),
            Some(ExitReason::StopLoss)
        );
    }
}
