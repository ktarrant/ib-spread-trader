//! Intraday level trades on the stock tape: fade or ride a break of the day's
//! initial balance (or of the prior session's value area), flat by the close.
//!
//! A separate path from the option engine — no chain, no forecast — that keeps
//! its execution rules: a decision is made on a 5-minute bar's close and fills at
//! the **next** bar's opening quote, buying at the ask and selling at the bid.
//! Targets and stops rest as orders once filled; a bar whose range reaches both
//! is scored as the stop, because a 5-minute bar cannot say which came first.
//!
//! Each side of the range (above the upper level, below the lower) runs one
//! state machine per session, evaluated on closes:
//!
//! - **Waiting** until a close lands clearly beyond the level (`retest_band`).
//! - **Outside**: a later close back within the band of the level is a retest,
//!   and a **breakout** entry in the direction of the break.
//! - **Extended**: the excursion reached `fade_min_extension` level-widths before
//!   any retest, so the breakout is abandoned and a **fade** is armed; RSI
//!   crossing back through its extreme, with price still beyond the level, fires
//!   it, targeting a return to the level.
//!
//! A close clearly back inside resets the side; a side that has signalled is done
//! for the session, taken or not. Inside the range nothing is traded.

use crate::data::Snapshot;
use crate::indicators::Rsi;
use crate::model::eval::bootstrap_interval;
use anyhow::{Context, Result, bail};
use chrono::{DateTime, NaiveDate, NaiveTime, Timelike, Utc};
use chrono_tz::America::New_York;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Levels {
    /// High and low of the first `ib_minutes` of the session.
    InitialBalance,
    /// VAL and VAH of the previous session's volume profile.
    PriorValueArea,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IntradayConfig {
    /// Empty means every stock with 5-minute bars in the snapshot.
    pub symbols: Vec<String>,
    pub levels: Levels,
    /// Length of the initial balance, from the 09:30 open.
    pub ib_minutes: u32,
    /// Share of the prior session's volume the value area holds.
    pub value_area: f64,
    /// How far past the level, in level-widths, the excursion must reach to arm
    /// a fade — and to abandon a breakout that never retested.
    pub fade_min_extension: f64,
    pub rsi_period: usize,
    /// A fade of an upside break fires when RSI crosses down through this; of a
    /// downside break, when it crosses up through `100 - rsi_level`.
    pub rsi_level: f64,
    /// Fade stop, in level-widths beyond the excursion's extreme.
    pub fade_stop_buffer: f64,
    /// A close within this many level-widths of the level, either side, is "at" it.
    pub retest_band: f64,
    /// Breakout target, in level-widths beyond the level.
    pub breakout_target: f64,
    /// Breakout stop, in level-widths back inside the level.
    pub breakout_stop: f64,
    /// No entries decided after this New York time, "HH:MM".
    pub last_entry: String,
    /// Dollars per trade; shares are rounded down.
    pub notional: f64,
    pub commission_per_share: f64,
    /// Per order, as IBKR's fixed schedule charges it.
    pub min_commission: f64,
    /// Refuse a breakout unless at least this many of its context labels
    /// (open vs prior value area, SPY, the 10:30 reaction) are `with` it.
    pub breakout_min_context: usize,
}

impl Default for IntradayConfig {
    fn default() -> Self {
        Self {
            symbols: Vec::new(),
            levels: Levels::InitialBalance,
            ib_minutes: 60,
            value_area: 0.70,
            fade_min_extension: 0.5,
            rsi_period: 14,
            rsi_level: 70.0,
            fade_stop_buffer: 0.1,
            retest_band: 0.1,
            breakout_target: 1.0,
            breakout_stop: 0.25,
            last_entry: "15:00".into(),
            notional: 10_000.0,
            commission_per_share: 0.005,
            min_commission: 1.0,
            breakout_min_context: 0,
        }
    }
}

impl IntradayConfig {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
    }

    /// Checks the config and returns the parsed entry cutoff.
    fn validate(&self) -> Result<NaiveTime> {
        if self.ib_minutes == 0 || !self.ib_minutes.is_multiple_of(5) {
            bail!("ib_minutes must be a positive multiple of 5 (the tape is 5-minute bars)");
        }
        if !(self.value_area > 0.0 && self.value_area <= 1.0) {
            bail!("value_area must be in (0, 1]");
        }
        if !(self.rsi_level > 50.0 && self.rsi_level < 100.0) {
            bail!("rsi_level must be in (50, 100); the downside mirror is 100 - rsi_level");
        }
        for (name, v) in [
            ("fade_min_extension", self.fade_min_extension),
            ("retest_band", self.retest_band),
            ("breakout_target", self.breakout_target),
            ("breakout_stop", self.breakout_stop),
            ("notional", self.notional),
        ] {
            if v.is_nan() || v <= 0.0 {
                bail!("{name} must be positive");
            }
        }
        if self.fade_min_extension <= self.retest_band {
            bail!("fade_min_extension must exceed retest_band, or no break is ever outside yet near");
        }
        NaiveTime::parse_from_str(&self.last_entry, "%H:%M")
            .with_context(|| format!("last_entry '{}' is not HH:MM", self.last_entry))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Quote {
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
}

/// One regular-hours 5-minute bar: trades OHLCV plus the bid and ask series.
#[derive(Debug, Clone, Copy)]
pub struct Bar {
    pub ts: DateTime<Utc>,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
    pub bid: Option<Quote>,
    pub ask: Option<Quote>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Setup {
    Breakout,
    Fade,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    Target,
    Stop,
    /// Flattened at the session's last quote.
    Close,
}

#[derive(Debug, Clone, Serialize)]
pub struct Trade {
    pub symbol: String,
    pub day: NaiveDate,
    pub setup: Setup,
    /// +1 long, -1 short.
    pub direction: i8,
    pub level: f64,
    pub width: f64,
    pub entry_ts: DateTime<Utc>,
    pub entry: f64,
    pub stop: f64,
    pub target: f64,
    pub exit_ts: DateTime<Utc>,
    pub exit: f64,
    pub exit_reason: ExitReason,
    pub shares: f64,
    pub net_pnl: f64,
    /// Net P&L over the dollars between entry and stop.
    pub net_r: f64,
    pub net_bp: f64,
    /// Today's level width over the trailing 20-session median. A narrow initial
    /// balance is the classic trend-day tell; this is known at entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub width_ratio: Option<f64>,
    #[serde(flatten)]
    pub features: Features,
    /// Where the session closed, in level-widths beyond the broken level.
    /// **Hindsight** — a diagnostic of day type, never an input.
    pub day_close_beyond: f64,
}

/// What was known when the decision was made, each read relative to the
/// trade's direction: `with` it, `against` it, `neutral`, or `unknown`.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct Features {
    /// The session's open against the prior session's value area.
    pub open_vs_prior_va: &'static str,
    /// SPY's close on the decision bar against SPY's own levels; `self` on SPY.
    pub spy: &'static str,
    /// The 10:30-11:00 move — the "reaction" to the opening range. `unknown`
    /// until 11:00.
    pub reaction_1030: &'static str,
    /// The 13:00-14:00 move — the post-lunch session. `unknown` until 14:00.
    pub reaction_1300: &'static str,
    /// When the decision bar closed.
    pub entry_window: &'static str,
}

impl Features {
    /// How many of the three day-type labels agree with the trade.
    pub fn context(&self) -> usize {
        [self.open_vs_prior_va, self.spy, self.reaction_1030]
            .iter()
            .filter(|&&l| l == "with" || l == "self")
            .count()
    }
}

fn relative(x: Option<f64>, dir: f64) -> &'static str {
    match x {
        None => "unknown",
        Some(v) if v * dir > 0.0 => "with",
        Some(v) if v * dir < 0.0 => "against",
        Some(_) => "neutral",
    }
}

/// Minutes since New York midnight at a bar's start.
fn minute(ts: DateTime<Utc>) -> u32 {
    let t = ts.with_timezone(&New_York).time();
    t.hour() * 60 + t.minute()
}

fn entry_window(decided_at: u32) -> &'static str {
    match decided_at {
        ..=630 => "09:30-10:30",
        631..=660 => "10:30-11:00",
        661..=780 => "11:00-13:00",
        781..=840 => "13:00-14:00",
        841..=900 => "14:00-15:00",
        _ => "15:00-16:00",
    }
}

#[derive(Debug, Default, Serialize)]
pub struct Census {
    pub symbols: usize,
    pub sessions: usize,
    pub sessions_without_levels: usize,
    /// Sides that closed clearly beyond their level, or reached fade distance.
    pub breaks: usize,
    pub breakout_signals: usize,
    pub fades_armed: usize,
    pub fade_signals: usize,
    pub signal_while_positioned: usize,
    pub after_last_entry: usize,
    /// Breakouts refused by `breakout_min_context`.
    pub context_refused: usize,
    pub unfillable: usize,
    pub crossed_quotes: usize,
    pub flattened_unquoted: usize,
}

const BARS_SQL: &str = "
SELECT b.ts, b.data_type, b.open, b.high, b.low, b.close, b.volume
FROM bars b JOIN contracts c ON c.id = b.contract_id
WHERE c.kind = 'stock' AND c.symbol = ?1 AND b.bar_size = '5min'
  AND b.data_type IN ('trades', 'bid', 'ask')
ORDER BY b.ts";

type Row = (f64, f64, f64, f64, f64);

fn load_bars(snapshot: &Snapshot, symbol: &str, census: &mut Census) -> Result<Vec<Bar>> {
    let mut stmt = snapshot.conn().prepare(BARS_SQL)?;
    let rows = stmt.query_map([symbol], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            (r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?, r.get(6)?),
        ))
    })?;
    let mut by_ts: BTreeMap<String, [Option<Row>; 3]> = BTreeMap::new();
    for row in rows {
        let (ts, kind, values): (String, String, Row) = row?;
        let slot = match kind.as_str() {
            "trades" => 0,
            "bid" => 1,
            _ => 2,
        };
        by_ts.entry(ts).or_default()[slot] = Some(values);
    }
    let quote = |r: Row| Quote { open: r.0, high: r.1, low: r.2, close: r.3 };
    let mut bars = Vec::new();
    for (ts, [trades, bid, ask]) in by_ts {
        let Some((open, high, low, close, volume)) = trades else { continue };
        let ts = DateTime::parse_from_rfc3339(&ts)
            .with_context(|| format!("timestamp '{ts}'"))?
            .with_timezone(&Utc);
        let local = ts.with_timezone(&New_York).time();
        if local < NaiveTime::from_hms_opt(9, 30, 0).unwrap()
            || local >= NaiveTime::from_hms_opt(16, 0, 0).unwrap()
        {
            continue;
        }
        let (mut bid, mut ask) = (bid.map(quote), ask.map(quote));
        if let (Some(b), Some(a)) = (bid, ask)
            && (b.open > a.open || b.close > a.close)
        {
            census.crossed_quotes += 1;
            (bid, ask) = (None, None);
        }
        bars.push(Bar { ts, open, high, low, close, volume, bid, ask });
    }
    Ok(bars)
}

fn stock_symbols(snapshot: &Snapshot) -> Result<Vec<String>> {
    let mut stmt = snapshot.conn().prepare(
        "SELECT DISTINCT symbol FROM contracts c WHERE kind = 'stock' AND EXISTS
           (SELECT 1 FROM bars b WHERE b.contract_id = c.id AND b.bar_size = '5min')
         ORDER BY symbol",
    )?;
    let rows = stmt.query_map([], |r| r.get(0))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// (VAL, VAH): the band grown from the point of control, one bin at a time
/// toward the heavier neighbour, until it holds `share` of the volume.
// ponytail: each 5-minute bar's volume is spread evenly over its range, which
// blurs the profile; 1-minute stock bars from the capture would sharpen it.
pub fn value_area(bars: &[Bar], share: f64) -> Option<(f64, f64)> {
    const BINS: usize = 200;
    let lo = bars.iter().map(|b| b.low).fold(f64::INFINITY, f64::min);
    let hi = bars.iter().map(|b| b.high).fold(f64::NEG_INFINITY, f64::max);
    if hi <= lo {
        return None;
    }
    let step = (hi - lo) / BINS as f64;
    let bin = |x: f64| (((x - lo) / step) as usize).min(BINS - 1);
    let mut vol = [0.0; BINS];
    for b in bars.iter().filter(|b| b.volume > 0.0) {
        let (a, z) = (bin(b.low), bin(b.high));
        let per = b.volume / (z - a + 1) as f64;
        vol[a..=z].iter_mut().for_each(|v| *v += per);
    }
    let total: f64 = vol.iter().sum();
    if total <= 0.0 {
        return None;
    }
    let poc = (0..BINS).max_by(|&i, &j| vol[i].total_cmp(&vol[j]))?;
    let (mut a, mut z, mut held) = (poc, poc, vol[poc]);
    while held < share * total {
        let up = (z + 1 < BINS).then(|| vol[z + 1]);
        let down = a.checked_sub(1).map(|i| vol[i]);
        match (down, up) {
            (Some(d), Some(u)) if u >= d => (z, held) = (z + 1, held + u),
            (Some(d), _) => (a, held) = (a - 1, held + d),
            (None, Some(u)) => (z, held) = (z + 1, held + u),
            (None, None) => break,
        }
    }
    Some((lo + a as f64 * step, lo + (z + 1) as f64 * step))
}

fn sessions(bars: &[Bar]) -> impl Iterator<Item = &[Bar]> {
    bars.chunk_by(|a, b| {
        a.ts.with_timezone(&New_York).date_naive() == b.ts.with_timezone(&New_York).date_naive()
    })
}

/// (lower, upper, first bar that may trade) for one session.
fn session_levels(
    cfg: &IntradayConfig,
    session: &[Bar],
    prior: Option<&[Bar]>,
) -> Option<(f64, f64, usize)> {
    let ib_bars = (cfg.ib_minutes / 5) as usize;
    let levels = match cfg.levels {
        Levels::InitialBalance => (session.len() > ib_bars && minute(session[0].ts) == 570)
            .then(|| {
                let ib = &session[..ib_bars];
                let lo = ib.iter().map(|b| b.low).fold(f64::INFINITY, f64::min);
                let hi = ib.iter().map(|b| b.high).fold(f64::NEG_INFINITY, f64::max);
                (lo, hi, ib_bars)
            }),
        Levels::PriorValueArea => {
            prior.and_then(|p| value_area(p, cfg.value_area)).map(|(lo, hi)| (lo, hi, 0))
        }
    };
    levels.filter(|(lo, hi, _)| hi > lo)
}

/// Each tradeable bar's close against its session's levels: +1 above, -1 below,
/// 0 inside. Read off SPY as the market's side of its own range.
fn level_positions(cfg: &IntradayConfig, bars: &[Bar]) -> HashMap<DateTime<Utc>, f64> {
    let mut out = HashMap::new();
    let mut prior = None;
    for session in sessions(bars) {
        if let Some((lo, hi, first)) = session_levels(cfg, session, prior) {
            for b in &session[first..] {
                let side = if b.close > hi { 1.0 } else if b.close < lo { -1.0 } else { 0.0 };
                out.insert(b.ts, side);
            }
        }
        prior = Some(session);
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum State {
    Waiting,
    Outside,
    Extended,
    Done,
}

/// One side of the range: `sign` +1 above the upper level, -1 below the lower.
struct Side {
    sign: f64,
    level: f64,
    state: State,
    extreme: Option<f64>,
}

struct Order {
    setup: Setup,
    dir: f64,
    level: f64,
    stop: f64,
    target: f64,
}

impl Side {
    fn update(
        &mut self,
        bar: &Bar,
        rsi: (Option<f64>, Option<f64>),
        width: f64,
        cfg: &IntradayConfig,
        census: &mut Census,
    ) -> Option<Order> {
        if self.state == State::Done {
            return None;
        }
        let s = self.sign;
        let beyond = |x: f64| s * (x - self.level) / width;
        let close = beyond(bar.close);
        if close < -cfg.retest_band {
            self.state = State::Waiting;
            self.extreme = None;
            return None;
        }
        let reach = if s > 0.0 { bar.high } else { bar.low };
        let extreme = self.extreme.map_or(reach, |e| if s > 0.0 { e.max(reach) } else { e.min(reach) });
        self.extreme = Some(extreme);
        let far = beyond(extreme) >= cfg.fade_min_extension;
        match self.state {
            State::Waiting | State::Outside if far => {
                if self.state == State::Waiting {
                    census.breaks += 1;
                }
                census.fades_armed += 1;
                self.state = State::Extended;
                None
            }
            State::Waiting if close > cfg.retest_band => {
                census.breaks += 1;
                self.state = State::Outside;
                None
            }
            State::Outside if close <= cfg.retest_band => {
                census.breakout_signals += 1;
                self.state = State::Done;
                Some(Order {
                    setup: Setup::Breakout,
                    dir: s,
                    level: self.level,
                    stop: self.level - s * cfg.breakout_stop * width,
                    target: self.level + s * cfg.breakout_target * width,
                })
            }
            State::Extended => {
                let (hi, lo) = (cfg.rsi_level, 100.0 - cfg.rsi_level);
                let turned = match rsi {
                    (Some(prev), Some(now)) if s > 0.0 => prev > hi && now <= hi,
                    (Some(prev), Some(now)) => prev < lo && now >= lo,
                    _ => false,
                };
                if !(turned && close > 0.0) {
                    return None;
                }
                census.fade_signals += 1;
                self.state = State::Done;
                Some(Order {
                    setup: Setup::Fade,
                    dir: -s,
                    level: self.level,
                    stop: extreme + s * cfg.fade_stop_buffer * width,
                    target: self.level,
                })
            }
            _ => None,
        }
    }
}

struct Open {
    order: Order,
    features: Features,
    width: f64,
    width_ratio: Option<f64>,
    entry_ts: DateTime<Utc>,
    entry: f64,
    shares: f64,
}

fn commission(cfg: &IntradayConfig, shares: f64) -> f64 {
    (shares * cfg.commission_per_share).max(cfg.min_commission)
}

/// Where and why a resting stop or target fills on this bar, closing against
/// the bid for a long and the ask for a short. Stop first when both are reached;
/// a gap through either fills at the open.
fn exit_on(p: &Open, bar: &Bar) -> Option<(f64, ExitReason)> {
    let o = &p.order;
    if o.dir > 0.0 {
        let q = bar.bid?;
        if q.low <= o.stop {
            return Some((q.open.min(o.stop), ExitReason::Stop));
        }
        if q.high >= o.target {
            return Some((q.open.max(o.target), ExitReason::Target));
        }
    } else {
        let q = bar.ask?;
        if q.high >= o.stop {
            return Some((q.open.max(o.stop), ExitReason::Stop));
        }
        if q.low <= o.target {
            return Some((q.open.min(o.target), ExitReason::Target));
        }
    }
    None
}

fn median(values: &[f64]) -> f64 {
    let mut v = values.to_vec();
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

/// Replay one symbol's regular-hours tape, session by session.
pub fn run_symbol(
    cfg: &IntradayConfig,
    symbol: &str,
    bars: &[Bar],
    market: Option<&HashMap<DateTime<Utc>, f64>>,
    census: &mut Census,
) -> Result<Vec<Trade>> {
    let cutoff = cfg.validate()?;
    // Fed every bar in order across sessions, as a chart would.
    let mut rsi = Rsi::new(cfg.rsi_period)?;
    let mut last_rsi = None;
    let mut widths: Vec<f64> = Vec::new();
    let mut prior: Option<&[Bar]> = None;
    let mut trades = Vec::new();

    for session in sessions(bars) {
        census.sessions += 1;
        let levels = session_levels(cfg, session, prior);
        let open_side = prior.and_then(|p| value_area(p, cfg.value_area)).map(|(val, vah)| {
            let open = session[0].open;
            if open > vah { 1.0 } else if open < val { -1.0 } else { 0.0 }
        });
        prior = Some(session);
        let Some((lower, upper, first)) = levels else {
            census.sessions_without_levels += 1;
            session.iter().for_each(|b| last_rsi = rsi.next(b.close));
            continue;
        };
        let width = upper - lower;
        let width_ratio = (widths.len() >= 5)
            .then(|| width / median(&widths[widths.len().saturating_sub(20)..]));
        widths.push(width);

        let mut sides = [
            Side { sign: 1.0, level: upper, state: State::Waiting, extreme: None },
            Side { sign: -1.0, level: lower, state: State::Waiting, extreme: None },
        ];
        let mut pending: Option<(Order, Features)> = None;
        let mut open: Option<Open> = None;
        let (mut at_1030, mut reaction_1030) = (None, None);
        let (mut at_1300, mut reaction_1300) = (None, None);
        let day_start = trades.len();
        let last = session.len() - 1;

        for (i, bar) in session.iter().enumerate() {
            let now = rsi.next(bar.close);
            let rsi_pair = (last_rsi, now);
            last_rsi = now;

            match minute(bar.ts) {
                625 => at_1030 = Some(bar.close),
                655 => reaction_1030 = at_1030.map(|p| bar.close - p),
                775 => at_1300 = Some(bar.close),
                835 => reaction_1300 = at_1300.map(|p| bar.close - p),
                _ => {}
            }

            if let Some((order, features)) = pending.take() {
                let price = if order.dir > 0.0 { bar.ask } else { bar.bid }.map(|q| q.open);
                let shares = price.map_or(0.0, |p| (cfg.notional / p).floor());
                match price {
                    Some(entry) if shares > 0.0 => {
                        open = Some(Open { order, features, width, width_ratio, entry_ts: bar.ts, entry, shares })
                    }
                    _ => census.unfillable += 1,
                }
            }

            if let Some(p) = &open {
                let exit = exit_on(p, bar).or_else(|| {
                    (i == last).then(|| {
                        let q = if p.order.dir > 0.0 { bar.bid } else { bar.ask };
                        let price = q.map(|q| q.close).unwrap_or_else(|| {
                            census.flattened_unquoted += 1;
                            bar.close
                        });
                        (price, ExitReason::Close)
                    })
                });
                if let Some((exit, reason)) = exit {
                    let p = open.take().unwrap();
                    let o = &p.order;
                    let net = o.dir * (exit - p.entry) * p.shares
                        - commission(cfg, p.shares) * 2.0;
                    trades.push(Trade {
                        symbol: symbol.to_string(),
                        day: bar.ts.with_timezone(&New_York).date_naive(),
                        setup: o.setup,
                        direction: o.dir as i8,
                        level: o.level,
                        width: p.width,
                        entry_ts: p.entry_ts,
                        entry: p.entry,
                        stop: o.stop,
                        target: o.target,
                        exit_ts: bar.ts,
                        exit,
                        exit_reason: reason,
                        shares: p.shares,
                        net_pnl: net,
                        net_r: net / ((p.entry - o.stop).abs() * p.shares),
                        net_bp: net / (p.entry * p.shares) * 1e4,
                        width_ratio: p.width_ratio,
                        features: p.features,
                        day_close_beyond: 0.0,
                    });
                }
            }

            if i < first || i == last {
                continue;
            }
            let late = bar.ts.with_timezone(&New_York).time() >= cutoff;
            for side in &mut sides {
                if let Some(order) = side.update(bar, rsi_pair, width, cfg, census) {
                    if open.is_some() || pending.is_some() {
                        census.signal_while_positioned += 1;
                    } else if late {
                        census.after_last_entry += 1;
                    } else {
                        let dir = order.dir;
                        let spy = match market {
                            _ if symbol == "SPY" => "self",
                            Some(m) => relative(m.get(&bar.ts).copied(), dir),
                            None => "unknown",
                        };
                        let features = Features {
                            open_vs_prior_va: relative(open_side, dir),
                            spy,
                            reaction_1030: relative(reaction_1030, dir),
                            reaction_1300: relative(reaction_1300, dir),
                            entry_window: entry_window(minute(bar.ts) + 5),
                        };
                        if order.setup == Setup::Breakout
                            && features.context() < cfg.breakout_min_context
                        {
                            census.context_refused += 1;
                            continue;
                        }
                        pending = Some((order, features));
                    }
                }
            }
        }
        let close = session[last].close;
        for t in &mut trades[day_start..] {
            let sign = if t.level == upper { 1.0 } else { -1.0 };
            t.day_close_beyond = sign * (close - t.level) / t.width;
        }
    }
    Ok(trades)
}

#[derive(Debug, Default, Serialize)]
pub struct Summary {
    pub trades: usize,
    pub win_rate: f64,
    pub net_pnl: f64,
    pub avg_net_bp: f64,
    /// 95 % interval on `avg_net_bp`, resampling whole days: symbols trade the
    /// same market on the same day, so trades within a day are not independent.
    pub avg_net_bp_interval: [f64; 2],
    pub days: usize,
    pub avg_r: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profit_factor: Option<f64>,
    pub target: usize,
    pub stop: usize,
    pub close: usize,
}

fn mean_of(sum: f64, n: f64) -> f64 {
    if n > 0.0 { sum / n } else { 0.0 }
}

fn summarize<'a>(trades: impl IntoIterator<Item = &'a Trade>) -> Summary {
    let mut s = Summary::default();
    let (mut won, mut lost) = (0.0, 0.0);
    let mut days: BTreeMap<NaiveDate, (f64, f64)> = BTreeMap::new();
    for t in trades {
        s.trades += 1;
        s.net_pnl += t.net_pnl;
        s.avg_net_bp += t.net_bp;
        s.avg_r += t.net_r;
        let day = days.entry(t.day).or_default();
        day.0 += t.net_bp;
        day.1 += 1.0;
        if t.net_pnl > 0.0 {
            s.win_rate += 1.0;
            won += t.net_pnl;
        } else {
            lost -= t.net_pnl;
        }
        match t.exit_reason {
            ExitReason::Target => s.target += 1,
            ExitReason::Stop => s.stop += 1,
            ExitReason::Close => s.close += 1,
        }
    }
    if s.trades > 0 {
        let n = s.trades as f64;
        s.win_rate /= n;
        s.avg_net_bp /= n;
        s.avg_r /= n;
    }
    s.profit_factor = (lost > 0.0).then(|| won / lost);
    s.days = days.len();
    s.avg_net_bp_interval =
        bootstrap_interval(&days.into_values().collect::<Vec<_>>(), mean_of);
    s
}

type BySetup<K> = BTreeMap<Setup, BTreeMap<K, Summary>>;

fn by_setup<K: Ord>(trades: &[Trade], key: impl Fn(&Trade) -> K) -> BySetup<K> {
    [Setup::Breakout, Setup::Fade]
        .into_iter()
        .map(|setup| {
            let mut groups: BTreeMap<K, Vec<&Trade>> = BTreeMap::new();
            for t in trades.iter().filter(|t| t.setup == setup) {
                groups.entry(key(t)).or_default().push(t);
            }
            (setup, groups.into_iter().map(|(k, v)| (k, summarize(v))).collect())
        })
        .collect()
}

/// Windows tested for follow-through: the opening hour, every half hour to
/// 15:30, and the post-lunch hour. Minutes since New York midnight.
const WINDOWS: [(u32, u32); 14] = [
    (570, 630),
    (570, 600),
    (600, 630),
    (630, 660),
    (660, 690),
    (690, 720),
    (720, 750),
    (750, 780),
    (780, 810),
    (810, 840),
    (780, 840),
    (840, 870),
    (870, 900),
    (900, 930),
];

/// Does a window's move predict the rest of the session? Measured on every
/// complete session of every symbol, not only the ones a setup traded.
#[derive(Debug, Serialize)]
pub struct WindowStat {
    pub window: String,
    pub sessions: usize,
    /// Share of sessions whose move from the window's end to the close went
    /// the window's way.
    pub follow_rate: f64,
    pub follow_rate_interval: [f64; 2],
    /// Mean move from the window's end to the close, signed by the window's
    /// direction, in bp — entering with the window at its end and holding to
    /// the bell, **before** ~4 bp of round-trip costs.
    pub follow_bp: f64,
    pub follow_bp_interval: [f64; 2],
}

/// Per window, per day: (hits, bp, sessions).
type WindowDays = Vec<BTreeMap<NaiveDate, (f64, f64, f64)>>;

fn window_moves(bars: &[Bar], acc: &mut WindowDays) {
    for session in sessions(bars) {
        let last = session.last().expect("chunks are non-empty");
        if session.len() != 78 || minute(session[0].ts) != 570 || minute(last.ts) != 955 {
            continue;
        }
        // Price at a boundary: the open of the bar starting there, or the close.
        let at = |m: u32| session.get(((m - 570) / 5) as usize).map_or(last.close, |b| b.open);
        let day = session[0].ts.with_timezone(&New_York).date_naive();
        for (w, &(from, to)) in WINDOWS.iter().enumerate() {
            let (a, b) = (at(from), at(to));
            let dir = (b - a).signum();
            if b == a {
                continue;
            }
            let follow = dir * (last.close - b) / b * 1e4;
            let cell = acc[w].entry(day).or_default();
            cell.0 += f64::from(u8::from(follow > 0.0));
            cell.1 += follow;
            cell.2 += 1.0;
        }
    }
}

fn window_stats(acc: WindowDays) -> Vec<WindowStat> {
    let hhmm = |m: u32| format!("{:02}:{:02}", m / 60, m % 60);
    WINDOWS
        .iter()
        .zip(acc)
        .map(|(&(from, to), days)| {
            let cells: Vec<_> = days.into_values().collect();
            let n: f64 = cells.iter().map(|c| c.2).sum();
            let hits: Vec<(f64, f64)> = cells.iter().map(|c| (c.0, c.2)).collect();
            let bps: Vec<(f64, f64)> = cells.iter().map(|c| (c.1, c.2)).collect();
            WindowStat {
                window: format!("{}-{}", hhmm(from), hhmm(to)),
                sessions: n as usize,
                follow_rate: mean_of(hits.iter().map(|c| c.0).sum(), n),
                follow_rate_interval: bootstrap_interval(&hits, mean_of),
                follow_bp: mean_of(bps.iter().map(|c| c.0).sum(), n),
                follow_bp_interval: bootstrap_interval(&bps, mean_of),
            }
        })
        .collect()
}

#[derive(Debug, Serialize)]
pub struct Report {
    pub config: IntradayConfig,
    pub census: Census,
    pub setups: BTreeMap<Setup, Summary>,
    /// Each entry-time feature, then the hindsight day-type label, bucketed per
    /// setup. Every feature except `day_close_hindsight` was known at entry.
    pub by_feature: BTreeMap<&'static str, BySetup<&'static str>>,
    pub by_symbol: BySetup<String>,
    pub session_windows: Vec<WindowStat>,
    pub caveats: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub trades: Vec<Trade>,
}

pub fn run(snapshot: &Snapshot, cfg: &IntradayConfig, include_trades: bool) -> Result<Report> {
    cfg.validate()?;
    let symbols = if cfg.symbols.is_empty() { stock_symbols(snapshot)? } else { cfg.symbols.clone() };
    let spy = load_bars(snapshot, "SPY", &mut Census::default())?;
    let market = (!spy.is_empty()).then(|| level_positions(cfg, &spy));
    let mut census = Census::default();
    let mut trades = Vec::new();
    let mut windows: WindowDays = vec![BTreeMap::new(); WINDOWS.len()];
    for symbol in &symbols {
        let bars = load_bars(snapshot, symbol, &mut census)?;
        if bars.is_empty() {
            bail!("{symbol} has no 5-minute stock bars in this snapshot");
        }
        census.symbols += 1;
        window_moves(&bars, &mut windows);
        trades.extend(run_symbol(cfg, symbol, &bars, market.as_ref(), &mut census)?);
    }

    let mut caveats = vec![
        "A 5-minute bar reaching both stop and target is scored as the stop.".to_string(),
        "Stops fill at the stop price (or the open, on a gap) with no slippage past the quote."
            .to_string(),
        "Intervals resample whole days; with ~80 days, buckets under ~20 days are anecdotes."
            .to_string(),
    ];
    if market.is_none() {
        caveats.push("No SPY tape in this snapshot; the `spy` feature is unknown throughout.".into());
    }
    for (setup, s) in by_setup(&trades, |_| ()) {
        let n = s.get(&()).map_or(0, |s| s.trades);
        if n < 100 {
            caveats.push(format!("{setup:?}: {n} trades; the win rate is descriptive, not a finding."));
        }
    }
    let by_feature = BTreeMap::from([
        ("width_ratio", by_setup(&trades, |t| match t.width_ratio {
            None => "unknown",
            Some(r) if r < 0.8 => "narrow_<0.8",
            Some(r) if r <= 1.25 => "normal",
            Some(_) => "wide_>1.25",
        })),
        ("open_vs_prior_va", by_setup(&trades, |t| t.features.open_vs_prior_va)),
        ("spy", by_setup(&trades, |t| t.features.spy)),
        ("reaction_1030", by_setup(&trades, |t| t.features.reaction_1030)),
        ("reaction_1300", by_setup(&trades, |t| t.features.reaction_1300)),
        ("entry_window", by_setup(&trades, |t| t.features.entry_window)),
        ("day_close_hindsight", by_setup(&trades, |t| match t.day_close_beyond {
            x if x < 0.0 => "back_inside",
            x if x < 1.0 => "beyond_<1w",
            _ => "trend_>=1w",
        })),
    ]);
    Ok(Report {
        config: cfg.clone(),
        census,
        setups: by_setup(&trades, |_| ())
            .into_iter()
            .map(|(k, mut v)| (k, v.remove(&()).unwrap_or_default()))
            .collect(),
        by_feature,
        by_symbol: by_setup(&trades, |t| t.symbol.clone()),
        session_windows: window_stats(windows),
        caveats,
        trades: if include_trades { trades } else { Vec::new() },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// A session at 09:30 New York on an EDT date, one bar per close.
    fn session(closes: &[f64]) -> Vec<Bar> {
        let open = Utc.with_ymd_and_hms(2026, 7, 15, 13, 30, 0).unwrap();
        closes
            .iter()
            .enumerate()
            .map(|(i, &c)| {
                let q = |d: f64| Quote { open: c + d, high: c + 0.05 + d, low: c - 0.05 + d, close: c + d };
                Bar {
                    ts: open + chrono::Duration::minutes(5 * i as i64),
                    open: c,
                    high: c + 0.05,
                    low: c - 0.05,
                    close: c,
                    volume: 1000.0,
                    bid: Some(q(-0.01)),
                    ask: Some(q(0.01)),
                }
            })
            .collect()
    }

    /// An IB climbing steadily 100 -> 101 (range 99.95..101.05, width 1.1), then
    /// `rest`, padded flat to 78 bars.
    fn day(rest: &[f64]) -> Vec<Bar> {
        let mut closes: Vec<f64> = (0..12).map(|i| 100.0 + i as f64 / 11.0).collect();
        closes.extend_from_slice(rest);
        let tail = *closes.last().unwrap();
        closes.resize(78, tail);
        session(&closes)
    }

    fn run(bars: &[Bar]) -> (Vec<Trade>, Census) {
        let mut census = Census::default();
        let trades = run_symbol(&IntradayConfig::default(), "T", bars, None, &mut census).unwrap();
        (trades, census)
    }

    #[test]
    fn a_retest_of_the_ib_high_buys_the_breakout_and_takes_the_target() {
        // Close clearly above 101.05, back to it, then run past 101.05 + 1.1.
        let (trades, census) = run(&day(&[101.3, 101.4, 101.1, 101.5, 101.9, 102.3]));
        assert_eq!(census.breakout_signals, 1);
        assert_eq!(trades.len(), 1, "{trades:?}");
        let t = &trades[0];
        assert_eq!((t.setup, t.direction, t.exit_reason), (Setup::Breakout, 1, ExitReason::Target));
        // Decided on the 101.1 close, filled at the next bar's opening ask.
        assert!((t.entry - 101.51).abs() < 1e-9);
        assert!(t.net_pnl > 0.0);
        // Decided on the 10:40 bar's close: before the 10:30-11:00 reaction is known.
        assert_eq!((t.features.entry_window, t.features.reaction_1030), ("10:30-11:00", "unknown"));
    }

    #[test]
    fn the_context_gate_refuses_a_breakout_nothing_supports() {
        // No SPY tape, no prior session, decided before 11:00: every label unknown.
        let cfg = IntradayConfig { breakout_min_context: 1, ..IntradayConfig::default() };
        let mut census = Census::default();
        let bars = day(&[101.3, 101.4, 101.1, 101.5, 101.9, 102.3]);
        let trades = run_symbol(&cfg, "T", &bars, None, &mut census).unwrap();
        assert!(trades.is_empty());
        assert_eq!((census.breakout_signals, census.context_refused), (1, 1));
    }

    #[test]
    fn a_session_that_rises_all_day_follows_through_from_every_window() {
        let closes: Vec<f64> = (0..78).map(|i| 100.0 + 0.1 * i as f64).collect();
        let mut acc: WindowDays = vec![BTreeMap::new(); WINDOWS.len()];
        window_moves(&session(&closes), &mut acc);
        let stats = window_stats(acc);
        assert!(stats.iter().all(|w| w.sessions == 1 && w.follow_rate == 1.0 && w.follow_bp > 0.0));
        assert_eq!(stats[3].window, "10:30-11:00");
    }

    #[test]
    fn a_run_away_from_the_ib_arms_a_fade_that_rsi_triggers() {
        // Straight up past 0.5 widths with no retest, then rolls over.
        let up: Vec<f64> = (1..=8).map(|i| 101.0 + 0.12 * i as f64).collect();
        let down: Vec<f64> = (1..=12).map(|i| 101.96 - 0.08 * i as f64).collect();
        let (trades, census) = run(&day(&[up, down].concat()));
        assert_eq!((census.fades_armed, census.breakout_signals), (1, 0));
        let t = trades.iter().find(|t| t.setup == Setup::Fade).expect("a fade");
        assert_eq!((t.direction, t.exit_reason), (-1, ExitReason::Target));
        assert!((t.target - 101.05).abs() < 1e-9 && t.entry > t.target);
    }

    #[test]
    fn the_value_area_brackets_where_the_volume_traded() {
        let mut bars = session(&[100.0; 20]);
        bars.push(session(&[105.0])[0]);
        bars.last_mut().unwrap().volume = 10.0;
        let (val, vah) = value_area(&bars, 0.7).unwrap();
        assert!(val < 100.0 && vah > 100.0 && vah < 101.0, "{val}..{vah}");
    }
}
