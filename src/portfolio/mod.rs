//! Cash, open spreads, and the trade log.
//!
//! Positions are marked at what they could actually be **closed** for — the
//! conservative exit credit — not at mid. One consequence is worth expecting
//! rather than debugging: equity drops the instant a trade opens, by the width
//! of the book. That is real. A position entered at the ask and marked at the
//! bid is genuinely worth less than it cost.

use anyhow::Result;
use chrono::{DateTime, NaiveDate, Utc};
use serde::Serialize;

use crate::data::ChainSnapshot;
use crate::execution::{Costs, FillError, entry_debit, exit_credit};
use crate::spread::{CONTRACT_MULTIPLIER, Side, Spread};

pub mod model;

pub use model::{IvScaledSizer, 
    AllocationCandidate, EqualWeightSizer, PortfolioModel, PortfolioModelKind, PortfolioState,
    ScoreWeightedSizer,
};

/// Why a position left the book.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    ProfitTarget,
    StopLoss,
    /// The underlying reached the forecast's target — the move the trade bet on
    /// has played out, so take it. Grounds the exit on the same forecast the
    /// entry used, and works for a naked long as well as a vertical.
    ForecastTarget,
    /// The underlying crossed the short strike — near max profit, close it.
    /// Only for debit structures; a credit spread's short strike is a loss, not a
    /// take-profit, so it is not closed on this.
    UnderlyingPastShortStrike,
    /// The forecast window has elapsed — the position has been open at least as
    /// long as the horizon it was scored to, so the forecast no longer speaks to
    /// it. Closes credit spreads at the end of their full forecast duration and
    /// stops anything else from drifting past the model's view.
    ForecastHorizon,
    /// A debit spread's live probability of profit fell below the floor — the
    /// give-up when the model no longer expects the trade to work out.
    PopFloor,
    /// The underlying left the bounds the entry forecast gave it, against the
    /// position. The give-up for a trade that has genuinely gone wrong, as
    /// opposed to one that is merely behind.
    ForecastStop,
    /// A naked long gave back too much of its peak gain — the trailing stop that
    /// lets a winner run while protecting the run.
    TrailingStop,
    /// Momentum turned against a naked long — the trend the entry leaned on has
    /// flipped, so the reason to hold is gone.
    MomentumExit,
    /// Too much of the time to expiry has elapsed.
    TimeDecay,
    /// Closed to avoid holding through an earnings announcement.
    BeforeEarnings,
    /// Closed on the first bar after a release it was held through: the move a
    /// long-vol position was bought for has happened, and so has the crush a
    /// short-vol one was paid for.
    AfterEarnings,
    /// Hard cutoff: close before getting too close to expiration.
    BeforeExpiry,
    /// Ran out of option bars and settled at intrinsic. Reported separately from
    /// traded P&L, because it is the one leg of the result the tape did not price.
    Expiry,
}

/// What the ranked model predicted for the trade it chose, carried onto the
/// position so predicted-vs-realized can be compared after the fact. `None` for
/// the staged pipeline, which makes no such prediction.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct TradePrediction {
    /// Model expected P/L at the scoring horizon, in dollars.
    pub expected_pl: f64,
    /// The rank score, `expected_pl / max_loss`.
    pub score: f64,
    /// Model probability the position clears its cost basis at the horizon.
    pub prob_profit: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct OpenPosition {
    pub id: usize,
    pub spread: Spread,
    /// The chain the spread was opened in. Every leg shares it, and only that
    /// chain's bars can mark, exit or close the position.
    pub expiry: NaiveDate,
    pub opened_at: DateTime<Utc>,
    /// Positive for a debit structure, negative for a credit one. Excludes commission.
    pub entry_debit: f64,
    pub entry_commission: f64,
    /// Best case at expiry net of entry cost, where the structure is bounded.
    pub max_profit: Option<f64>,
    /// Worst case at expiry net of entry cost, as a positive number of dollars.
    pub max_loss: Option<f64>,
    /// The ranked model's prediction for this trade, if it came from that path.
    pub prediction: Option<TradePrediction>,
    /// The underlying price at which the forecast-target exit closes this
    /// position (a favorable quantile of the entry forecast). `None` unless the
    /// ranked config enabled it.
    pub forecast_target: Option<f64>,
    /// The mirror: the *adverse* quantile of the entry forecast, past which the
    /// underlying has left the bounds this trade was underwritten inside.
    /// `None` unless the exit bundle enabled it.
    pub forecast_stop: Option<f64>,
    /// What the model saw on the bar it decided on. `None` for the staged path,
    /// which has no feature engine.
    pub entry_features: Option<crate::model::EntryFeatures>,
    /// Each leg's entry implied vol (sticky-strike), carried from the chosen
    /// candidate. Empty for the staged path. Lets the live-POP exit re-score the
    /// spread against the current distribution with the vols it was priced at.
    pub leg_vols: Vec<f64>,
    /// Running peak unrealized P&L since entry, updated each bar. Drives the
    /// naked-long trailing stop. `None` until the first mark.
    pub peak_value: Option<f64>,
}

impl OpenPosition {
    /// P&L if closed against this bar, net of both commissions.
    pub fn unrealized(&self, bar: &ChainSnapshot, costs: &Costs) -> Result<f64, FillError> {
        let credit = exit_credit(&self.spread, bar)?;
        Ok(credit - self.entry_debit - self.entry_commission - costs.commission(&self.spread))
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ClosedTrade {
    pub id: usize,
    pub symbol: String,
    pub expiry: NaiveDate,
    pub description: String,
    pub opened_at: DateTime<Utc>,
    pub closed_at: DateTime<Utc>,
    pub bars_held: usize,
    pub entry_debit: f64,
    pub exit_credit: f64,
    pub commission: f64,
    /// `exit_credit - entry_debit - commission`.
    pub pnl: f64,
    pub exit_reason: ExitReason,
    /// True when the exit was priced at intrinsic because option quotes were
    /// unavailable. The P&L is an estimate, not a market observation.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub synthetic_price: bool,
    /// Structural worst case at entry, net of cost — the capital at risk, and the
    /// denominator of realized P/L-per-risk. `None` for an unbounded structure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_loss: Option<f64>,
    /// The ranked model's prediction at entry, to compare against realized `pnl`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub prediction: Option<TradePrediction>,
    /// The forecast-target price the position was set to close at, for the chart.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forecast_target: Option<f64>,
    /// The adverse bound it was set to stop at, for the same chart.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forecast_stop: Option<f64>,
    /// The feature vector the entry decision was made on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entry_features: Option<crate::model::EntryFeatures>,
    /// Open across an earnings release: opened on or before the last session
    /// before it, closed after. Such a trade's P&L is one binary event's, so the
    /// report counts it apart. Set by the engine once the run is over.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub held_through_earnings: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Portfolio {
    pub cash: f64,
    pub initial_cash: f64,
    pub open: Vec<OpenPosition>,
    pub closed: Vec<ClosedTrade>,
    next_id: usize,
}

impl Portfolio {
    pub fn new(initial_cash: f64) -> Self {
        Self {
            cash: initial_cash,
            initial_cash,
            open: Vec::new(),
            closed: Vec::new(),
            next_id: 1,
        }
    }

    /// A portfolio holding only these already-closed trades and no open
    /// positions — for assembling a per-symbol *view* of a shared-account run,
    /// where the real cash lives on the one shared portfolio.
    pub fn from_closed(initial_cash: f64, closed: Vec<ClosedTrade>) -> Self {
        Self {
            cash: initial_cash,
            initial_cash,
            open: Vec::new(),
            closed,
            next_id: 1,
        }
    }

    /// Total capital committed to risk across the open positions — the sum of
    /// each position's max loss. This is what a capital allocator sizes against:
    /// cash paid for a debit, margin posted for a credit, both captured by
    /// `max_loss`. A position with no bounded max loss (a naked long call) risks
    /// only the premium it paid, so it falls back to its entry debit.
    pub fn capital_at_risk(&self) -> f64 {
        self.open
            .iter()
            .map(|p| p.max_loss.unwrap_or_else(|| p.entry_debit.max(0.0)))
            .sum()
    }

    /// Update each open position's running peak gross P&L against `bar` — the
    /// state the naked-long trailing stop reads. A position whose legs are not
    /// quoted on this bar (a different symbol, or a gap in the tape) keeps its
    /// last peak, so calling this with one symbol's bar only touches that
    /// symbol's positions.
    pub fn update_peaks(&mut self, bar: &ChainSnapshot, _costs: &Costs) {
        for p in &mut self.open {
            if let Ok(credit) = exit_credit(&p.spread, bar) {
                let gross = credit - p.entry_debit;
                p.peak_value = Some(p.peak_value.map_or(gross, |peak| peak.max(gross)));
            }
        }
    }

    /// Open a spread against `bar`, paying the conservative debit plus commission.
    pub fn open(
        &mut self,
        spread: Spread,
        bar: &ChainSnapshot,
        costs: &Costs,
    ) -> Result<usize, FillError> {
        let debit = entry_debit(&spread, bar)?;
        let commission = costs.commission(&spread);
        let bounds = spread.payoff_bounds();

        self.cash -= debit + commission;
        let id = self.next_id;
        self.next_id += 1;
        self.open.push(OpenPosition {
            // `payoff_bounds` is before entry cost; net it here so `max_profit`
            // means what a profit target is a percentage *of*.
            max_profit: bounds.map(|(_, hi)| hi - debit),
            max_loss: bounds.map(|(lo, _)| debit - lo),
            id,
            spread,
            expiry: bar.expiry,
            opened_at: bar.ts,
            entry_debit: debit,
            entry_commission: commission,
            prediction: None,
            forecast_target: None,
            forecast_stop: None,
            entry_features: None,
            leg_vols: Vec::new(),
            peak_value: None,
        });
        Ok(id)
    }

    /// Close a position against `bar`.
    pub fn close(
        &mut self,
        id: usize,
        bar: &ChainSnapshot,
        costs: &Costs,
        reason: ExitReason,
        bars_held: usize,
    ) -> Result<Option<ClosedTrade>, FillError> {
        let Some(index) = self.open.iter().position(|p| p.id == id) else {
            return Ok(None);
        };
        let credit = exit_credit(&self.open[index].spread, bar)?;
        let position = self.open.remove(index);
        let commission = position.entry_commission + costs.commission(&position.spread);

        self.cash += credit - costs.commission(&position.spread);
        let trade = ClosedTrade {
            id: position.id,
            symbol: position.spread.symbol.clone(),
            expiry: position.expiry,
            description: position.spread.describe(),
            opened_at: position.opened_at,
            closed_at: bar.ts,
            bars_held,
            entry_debit: position.entry_debit,
            exit_credit: credit,
            commission,
            pnl: credit - position.entry_debit - commission,
            exit_reason: reason,
            synthetic_price: false,
            max_loss: position.max_loss,
            prediction: position.prediction,
            forecast_target: position.forecast_target,
            forecast_stop: position.forecast_stop,
            entry_features: position.entry_features,
            held_through_earnings: false,
        };
        self.closed.push(trade.clone());
        Ok(Some(trade))
    }

    /// Close a position at intrinsic when option quotes are unavailable.
    ///
    /// This is the mid-tape equivalent of settlement: the option market stopped
    /// making a quote in one or both legs, but the underlying is still quoted
    /// and a time-based exit rule (before-expiry, time-decay) has fired.
    /// Commission is charged — unlike settlement, this is a decision to close.
    pub fn close_at_intrinsic(
        &mut self,
        id: usize,
        underlying: f64,
        bar_ts: DateTime<Utc>,
        costs: &Costs,
        reason: ExitReason,
        bars_held: usize,
    ) -> Option<ClosedTrade> {
        let index = self.open.iter().position(|p| p.id == id)?;
        let position = self.open.remove(index);
        let credit = position.spread.payoff_at(underlying);
        let exit_commission = costs.commission(&position.spread);
        let commission = position.entry_commission + exit_commission;

        self.cash += credit - exit_commission;
        let trade = ClosedTrade {
            id: position.id,
            symbol: position.spread.symbol.clone(),
            expiry: position.expiry,
            description: position.spread.describe(),
            opened_at: position.opened_at,
            closed_at: bar_ts,
            bars_held,
            entry_debit: position.entry_debit,
            exit_credit: credit,
            commission,
            pnl: credit - position.entry_debit - commission,
            exit_reason: reason,
            synthetic_price: true,
            max_loss: position.max_loss,
            prediction: position.prediction,
            forecast_target: position.forecast_target,
            forecast_stop: position.forecast_stop,
            entry_features: position.entry_features,
            held_through_earnings: false,
        };
        self.closed.push(trade.clone());
        Some(trade)
    }

    /// Settle a position at intrinsic against the underlying's settlement price.
    ///
    /// The option tape stops the day *before* expiry — the reference snapshot's
    /// last option bar is 2026-08-20 for a 2026-08-21 chain — so a position that
    /// never hit a target or a stop cannot be closed on a quote. It is settled
    /// against the stock's own print on expiry day, which the capture does have.
    ///
    /// No commission is charged: IB does not bill exercise or assignment on US
    /// equity options, and a spread settled to cash never becomes stock here.
    /// That does mean a deep-ITM spread settles more cleanly than it would in
    /// life, where both legs assign and pin risk is real.
    pub fn settle_at_expiry(
        &mut self,
        id: usize,
        underlying: f64,
        settled_at: DateTime<Utc>,
        bars_held: usize,
    ) -> Option<ClosedTrade> {
        let index = self.open.iter().position(|p| p.id == id)?;
        let position = self.open.remove(index);
        let credit = position.spread.payoff_at(underlying);

        self.cash += credit;
        let trade = ClosedTrade {
            id: position.id,
            symbol: position.spread.symbol.clone(),
            expiry: position.expiry,
            description: position.spread.describe(),
            opened_at: position.opened_at,
            closed_at: settled_at,
            bars_held,
            entry_debit: position.entry_debit,
            exit_credit: credit,
            commission: position.entry_commission,
            pnl: credit - position.entry_debit - position.entry_commission,
            exit_reason: ExitReason::Expiry,
            synthetic_price: false,
            max_loss: position.max_loss,
            prediction: position.prediction,
            forecast_target: position.forecast_target,
            forecast_stop: position.forecast_stop,
            entry_features: position.entry_features,
            held_through_earnings: false,
        };
        self.closed.push(trade.clone());
        Some(trade)
    }

    /// Cash plus what every open position would fetch if closed against `bar`.
    ///
    /// Positions the bar cannot price are held at their entry cost rather than
    /// dropped — marking them to zero would invent a loss the market never showed.
    pub fn equity(&self, bar: &ChainSnapshot, costs: &Costs) -> f64 {
        self.cash
            + self
                .open
                .iter()
                .map(|p| match exit_credit(&p.spread, bar) {
                    Ok(credit) => credit - costs.commission(&p.spread),
                    Err(_) => p.entry_debit,
                })
                .sum::<f64>()
    }

    pub fn position(&self, id: usize) -> Option<&OpenPosition> {
        self.open.iter().find(|p| p.id == id)
    }

    /// Total P&L split by whether the tape priced the exit or intrinsic did.
    pub fn realized_split(&self) -> (f64, f64) {
        let mut traded = 0.0;
        let mut settled = 0.0;
        for trade in &self.closed {
            if trade.exit_reason == ExitReason::Expiry {
                settled += trade.pnl;
            } else {
                traded += trade.pnl;
            }
        }
        (traded, settled)
    }
}

/// Intrinsic value of one leg at settlement, in dollars. Exposed for reporting.
pub fn leg_intrinsic(
    strike: f64,
    is_call: bool,
    underlying: f64,
    quantity: u32,
    side: Side,
) -> f64 {
    let intrinsic = if is_call {
        (underlying - strike).max(0.0)
    } else {
        (strike - underlying).max(0.0)
    };
    side.sign() * intrinsic * quantity as f64 * CONTRACT_MULTIPLIER
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data::chain::{OptionQuote, UnderlyingBar};
    use crate::domain::OptionRight;
    use crate::spread::Leg;
    use chrono::TimeZone;

    fn quote(contract_id: i64, strike: f64, bid: f64, ask: f64) -> OptionQuote {
        OptionQuote {
            contract_id,
            strike,
            right: OptionRight::Call,
            bid,
            ask,
            mid: 0.5 * (bid + ask),
            implied_vol: Some(0.2),
            delta: Some(0.5),
        }
    }

    fn bar_at(hour: u32, long: (f64, f64), short: (f64, f64)) -> ChainSnapshot {
        ChainSnapshot {
            symbol: "SPY".into(),
            expiry: chrono::NaiveDate::from_ymd_opt(2026, 8, 21).unwrap(),
            ts: Utc.with_ymd_and_hms(2026, 8, 19, hour, 0, 0).unwrap(),
            underlying: UnderlyingBar {
                mid: Some(751.0),
                close: Some(751.0),
                ..UnderlyingBar::default()
            },
            forward: 751.0,
            residual_mad: 0.02,
            paired_strikes: 30,
            year_fraction: 0.006,
            quotes: vec![
                quote(1, 750.0, long.0, long.1),
                quote(2, 755.0, short.0, short.1),
            ],
        }
    }

    fn bull_call() -> Spread {
        Spread::new(
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
        )
    }

    #[test]
    fn opening_charges_the_debit_and_the_commission() {
        let mut p = Portfolio::new(10_000.0);
        let bar = bar_at(15, (2.90, 3.10), (1.40, 1.60));
        p.open(bull_call(), &bar, &Costs::default()).unwrap();

        // 3.10 - 1.40 = 1.70 debit, plus $1.30 of commission.
        assert!(
            (p.cash - (10_000.0 - 170.0 - 1.30)).abs() < 1e-9,
            "{}",
            p.cash
        );
        assert_eq!(p.open.len(), 1);
    }

    /// Max profit is net of what was paid, because that is what a profit target
    /// is a percentage of. Width $500, debit $170, so $330 is the best case.
    #[test]
    fn bounds_are_recorded_net_of_entry_cost() {
        let mut p = Portfolio::new(10_000.0);
        let bar = bar_at(15, (2.90, 3.10), (1.40, 1.60));
        let id = p.open(bull_call(), &bar, &Costs::default()).unwrap();
        let pos = p.position(id).unwrap();
        assert_eq!(pos.max_profit, Some(330.0));
        assert_eq!(pos.max_loss, Some(170.0));
    }

    /// The expected, correct consequence of marking at the exit price: opening a
    /// position costs the book's width immediately.
    #[test]
    fn equity_drops_by_the_spread_the_moment_a_trade_opens() {
        let costs = Costs::default();
        let mut p = Portfolio::new(10_000.0);
        let bar = bar_at(15, (2.90, 3.10), (1.40, 1.60));
        p.open(bull_call(), &bar, &costs).unwrap();

        // Paid 170 to open, could close for 130: $40 of book plus $2.60 of
        // round-trip commission.
        assert!((p.equity(&bar, &costs) - (10_000.0 - 40.0 - 2.60)).abs() < 1e-9);
    }

    #[test]
    fn closing_a_winner_books_the_pnl_and_both_commissions() {
        let costs = Costs::default();
        let mut p = Portfolio::new(10_000.0);
        let id = p
            .open(bull_call(), &bar_at(15, (2.90, 3.10), (1.40, 1.60)), &costs)
            .unwrap();

        // The spread widens. Closing takes the long leg's bid (4.00) and pays
        // the short leg's ask (1.20) — not its bid — for a $280 credit.
        let later = bar_at(17, (4.00, 4.20), (1.00, 1.20));
        let trade = p
            .close(id, &later, &costs, ExitReason::ProfitTarget, 4)
            .unwrap()
            .unwrap();

        assert_eq!(trade.entry_debit, 170.0);
        assert_eq!(trade.exit_credit, 280.0);
        assert!((trade.commission - 2.60).abs() < 1e-9);
        assert!((trade.pnl - (280.0 - 170.0 - 2.60)).abs() < 1e-9);
        assert_eq!(trade.bars_held, 4);
        assert!(p.open.is_empty());
        assert!((p.cash - (10_000.0 + 107.40)).abs() < 1e-9);
    }

    /// Cash and the trade log have to agree, or the equity curve is fiction.
    #[test]
    fn cash_after_a_round_trip_equals_initial_plus_pnl() {
        let costs = Costs::default();
        let mut p = Portfolio::new(10_000.0);
        let id = p
            .open(bull_call(), &bar_at(15, (2.90, 3.10), (1.40, 1.60)), &costs)
            .unwrap();
        let trade = p
            .close(
                id,
                &bar_at(17, (4.00, 4.20), (1.00, 1.20)),
                &costs,
                ExitReason::StopLoss,
                4,
            )
            .unwrap()
            .unwrap();
        assert!((p.cash - (p.initial_cash + trade.pnl)).abs() < 1e-9);
    }

    /// A spread that never hit a target or a stop settles at intrinsic against
    /// the stock's expiry-day print, since the option tape stopped a day early.
    #[test]
    fn expiry_settles_a_winner_at_full_width() {
        let costs = Costs::default();
        let mut p = Portfolio::new(10_000.0);
        let id = p
            .open(bull_call(), &bar_at(15, (2.90, 3.10), (1.40, 1.60)), &costs)
            .unwrap();

        let settled_at = Utc.with_ymd_and_hms(2026, 8, 21, 20, 0, 0).unwrap();
        let trade = p.settle_at_expiry(id, 800.0, settled_at, 40).unwrap();

        assert_eq!(trade.exit_credit, 500.0, "both legs ITM: the full width");
        assert_eq!(trade.exit_reason, ExitReason::Expiry);
        // Only the entry commission — nothing is billed to settle.
        assert!((trade.commission - 1.30).abs() < 1e-9);
        assert!((trade.pnl - (500.0 - 170.0 - 1.30)).abs() < 1e-9);
    }

    #[test]
    fn expiry_settles_a_loser_at_zero() {
        let costs = Costs::default();
        let mut p = Portfolio::new(10_000.0);
        let id = p
            .open(bull_call(), &bar_at(15, (2.90, 3.10), (1.40, 1.60)), &costs)
            .unwrap();
        let settled_at = Utc.with_ymd_and_hms(2026, 8, 21, 20, 0, 0).unwrap();
        let trade = p.settle_at_expiry(id, 700.0, settled_at, 40).unwrap();

        assert_eq!(trade.exit_credit, 0.0);
        assert!((trade.pnl - (-171.30)).abs() < 1e-9);
        assert!(
            (trade.pnl.abs() - 171.30).abs() < 1e-9,
            "capped at the debit plus commission"
        );
    }

    /// Settling between the strikes is where a spread's payoff is actually
    /// interesting, and where an off-by-one in the leg signs would show up.
    #[test]
    fn expiry_settles_between_the_strikes_proportionally() {
        let costs = Costs::default();
        let mut p = Portfolio::new(10_000.0);
        let id = p
            .open(bull_call(), &bar_at(15, (2.90, 3.10), (1.40, 1.60)), &costs)
            .unwrap();
        let settled_at = Utc.with_ymd_and_hms(2026, 8, 21, 20, 0, 0).unwrap();
        let trade = p.settle_at_expiry(id, 752.5, settled_at, 40).unwrap();
        assert_eq!(trade.exit_credit, 250.0);
    }

    /// Settlement P&L is the part of the result the tape never priced, so it is
    /// reported apart from trades that closed on a real quote.
    #[test]
    fn realized_pnl_separates_traded_from_settled() {
        let costs = Costs::default();
        let mut p = Portfolio::new(10_000.0);
        let open_bar = bar_at(15, (2.90, 3.10), (1.40, 1.60));

        let a = p.open(bull_call(), &open_bar, &costs).unwrap();
        p.close(
            a,
            &bar_at(17, (4.00, 4.20), (1.00, 1.20)),
            &costs,
            ExitReason::ProfitTarget,
            4,
        )
        .unwrap();

        let b = p.open(bull_call(), &open_bar, &costs).unwrap();
        p.settle_at_expiry(
            b,
            800.0,
            Utc.with_ymd_and_hms(2026, 8, 21, 20, 0, 0).unwrap(),
            40,
        );

        let (traded, settled) = p.realized_split();
        assert!((traded - 107.40).abs() < 1e-9, "{traded}");
        assert!((settled - 328.70).abs() < 1e-9);
        assert_eq!(p.closed.len(), 2);
    }

    /// A wing that stops printing must not be marked to zero — that would invent
    /// a total loss the market never showed.
    #[test]
    fn an_unpriceable_position_is_held_at_cost_not_written_off() {
        let costs = Costs::default();
        let mut p = Portfolio::new(10_000.0);
        p.open(bull_call(), &bar_at(15, (2.90, 3.10), (1.40, 1.60)), &costs)
            .unwrap();

        let mut blank = bar_at(17, (4.00, 4.20), (1.00, 1.20));
        blank.quotes.clear();
        assert!((p.equity(&blank, &costs) - (10_000.0 - 1.30)).abs() < 1e-9);
    }

    #[test]
    fn closing_an_unknown_position_is_not_an_error() {
        let mut p = Portfolio::new(10_000.0);
        let bar = bar_at(15, (2.90, 3.10), (1.40, 1.60));
        assert!(
            p.close(42, &bar, &Costs::default(), ExitReason::StopLoss, 0)
                .unwrap()
                .is_none()
        );
        assert_eq!(p.cash, 10_000.0);
    }
}
