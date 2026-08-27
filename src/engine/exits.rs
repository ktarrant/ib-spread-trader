//! When to close.
//!
//! Both rules are measured on **gross** P&L — exit credit minus entry debit,
//! before commission. That is not an oversight: a profit target is a resting
//! limit order at a spread *price*, and no order can be placed that accounts for
//! its own commission. Commissions are charged in full and reported in the net
//! P&L; they simply are not what the trigger reads.

use serde::Serialize;

use crate::portfolio::{ExitReason, OpenPosition};

#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct ExitRules {
    /// Close once gross P&L reaches this percentage of max profit. Requires a
    /// bounded structure — there is no percentage of an infinite maximum.
    pub profit_target_pct: Option<f64>,
    /// Close once gross P&L falls to minus this percentage of the debit paid.
    /// 100 means "hold until the spread is worthless", which for a debit
    /// vertical is its true max loss.
    pub stop_loss_pct: Option<f64>,
}

impl ExitRules {
    /// Which rule, if any, `gross` P&L has tripped.
    ///
    /// The profit target is tested first. On a bar that somehow satisfies both,
    /// a winner should not be logged as a stop-out.
    pub fn triggered(&self, position: &OpenPosition, gross: f64) -> Option<ExitReason> {
        if let Some(pct) = self.profit_target_pct
            && let Some(max_profit) = position.max_profit
            && max_profit > 0.0
            && gross >= max_profit * pct / 100.0
        {
            return Some(ExitReason::ProfitTarget);
        }
        if let Some(pct) = self.stop_loss_pct
            && position.entry_debit > 0.0
            && gross <= -position.entry_debit * pct / 100.0
        {
            return Some(ExitReason::StopLoss);
        }
        None
    }
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

    fn rules() -> ExitRules {
        ExitRules {
            profit_target_pct: Some(50.0),
            stop_loss_pct: Some(100.0),
        }
    }

    #[test]
    fn the_profit_target_fires_at_half_of_max_profit() {
        assert_eq!(rules().triggered(&position(), 164.0), None);
        assert_eq!(
            rules().triggered(&position(), 165.0),
            Some(ExitReason::ProfitTarget)
        );
    }

    #[test]
    fn a_full_stop_fires_when_the_spread_is_worthless() {
        assert_eq!(rules().triggered(&position(), -169.0), None);
        assert_eq!(
            rules().triggered(&position(), -170.0),
            Some(ExitReason::StopLoss)
        );
    }

    #[test]
    fn a_half_stop_fires_at_half_the_debit() {
        let rules = ExitRules {
            profit_target_pct: None,
            stop_loss_pct: Some(50.0),
        };
        assert_eq!(rules.triggered(&position(), -84.0), None);
        assert_eq!(
            rules.triggered(&position(), -85.0),
            Some(ExitReason::StopLoss)
        );
    }

    #[test]
    fn a_quiet_position_is_left_alone() {
        assert_eq!(rules().triggered(&position(), 0.0), None);
        assert_eq!(rules().triggered(&position(), 20.0), None);
    }

    /// Gross, not net: at exactly the target the position has made $165 before
    /// the $2.60 of commission that will come off it. Reading net here would
    /// make the target unreachable by the amount of a cost no order can price.
    #[test]
    fn the_target_reads_gross_pnl_not_net() {
        let p = position();
        let gross = 165.0;
        assert_eq!(rules().triggered(&p, gross), Some(ExitReason::ProfitTarget));

        let net = gross - p.entry_commission - 1.30;
        assert!(net < 165.0);
        assert_eq!(
            rules().triggered(&p, net),
            None,
            "net would not have tripped it"
        );
    }

    /// An unbounded structure has no max profit to take a percentage of, so the
    /// target simply never fires. The stop still works, since it reads the debit.
    #[test]
    fn an_unbounded_position_has_no_profit_target_but_keeps_its_stop() {
        let mut p = position();
        p.max_profit = None;
        assert_eq!(rules().triggered(&p, 100_000.0), None);
        assert_eq!(rules().triggered(&p, -200.0), Some(ExitReason::StopLoss));
    }

    #[test]
    fn no_rules_means_no_exits() {
        let none = ExitRules::default();
        assert_eq!(none.triggered(&position(), 10_000.0), None);
        assert_eq!(none.triggered(&position(), -10_000.0), None);
    }

    /// A bar that satisfies both must log as the win it is.
    #[test]
    fn the_profit_target_wins_a_tie() {
        let both = ExitRules {
            profit_target_pct: Some(1.0),
            stop_loss_pct: Some(0.0),
        };
        assert_eq!(
            both.triggered(&position(), 5.0),
            Some(ExitReason::ProfitTarget)
        );
    }
}
