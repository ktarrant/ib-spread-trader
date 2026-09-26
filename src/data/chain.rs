//! One symbol/expiry loaded once, then replayed bar by bar.
//!
//! Two things shape this module.
//!
//! **Load once, borrow forever.** The parked `algorithm-api` branch in
//! `~/ib-option-calc` cloned its history, quote map and contract map into a fresh
//! context on every bar — O(n²) over ~1M rows. Here the whole chain is read into
//! one sorted `Vec` up front and every per-bar view is a slice into it.
//!
//! **Losses are counted, never hidden.** A quote with only one side, a crossed
//! book, a non-positive mid — each is dropped and tallied in [`Quality`], which
//! the run report prints. Silently discarding them would make a thin chain look
//! like a clean one.

use anyhow::{Context, Result};
use chrono::{DateTime, Datelike, NaiveDate, Utc};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

use crate::domain::{OptionRight, normalize_symbol};
use crate::pricing::{self, PairedStrike};

use super::catalog::BAR_SIZE;
use super::snapshot::Snapshot;

/// What was dropped on the way to a clean two-sided series. Surfaced, never hidden.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Quality {
    pub rows_read: usize,
    /// Only one side of the market printed. Cannot be filled against.
    pub dropped_unpaired: usize,
    /// `bid > ask`. Rare (7 rows in 24 330 on SPY calls) but real.
    pub dropped_crossed: usize,
    /// `mid <= 0`.
    pub dropped_nonpositive: usize,
}

impl Quality {
    pub fn rows_kept(&self) -> usize {
        self.rows_read - self.dropped_unpaired - self.dropped_crossed - self.dropped_nonpositive
    }
}

/// One option's two-sided quote at one timestamp, before any maths.
#[derive(Debug, Clone, Copy)]
struct RawQuote {
    ts: DateTime<Utc>,
    contract_id: i64,
    strike: f64,
    right: OptionRight,
    bid: f64,
    ask: f64,
}

/// The underlying at one timestamp.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct UnderlyingBar {
    /// Bid/ask midpoint — the like-for-like comparison against an option mid.
    pub mid: Option<f64>,
    /// OHLCV from the underlying's `trades` row only. Volume on bid/ask rows is
    /// a capture sentinel and must never leak into a signal.
    pub open: Option<f64>,
    pub high: Option<f64>,
    pub low: Option<f64>,
    pub close: Option<f64>,
    pub volume: Option<f64>,
}

impl UnderlyingBar {
    /// Best available price, preferring the trades close.
    pub fn price(&self) -> Option<f64> {
        self.close.or(self.mid)
    }
}

/// One option, priced and with its greeks resolved, at one timestamp.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct OptionQuote {
    pub contract_id: i64,
    pub strike: f64,
    pub right: OptionRight,
    pub bid: f64,
    pub ask: f64,
    pub mid: f64,
    /// `None` where no vol solves — a quote at or below intrinsic, or above its
    /// ceiling. Such a strike is **unusable**, not zero-vol.
    pub implied_vol: Option<f64>,
    /// `None` exactly when `implied_vol` is.
    pub delta: Option<f64>,
}

impl OptionQuote {
    /// What a buyer pays to open, or a seller pays to close.
    pub fn ask(&self) -> f64 {
        self.ask
    }
    /// What a seller receives to open, or a buyer receives to close.
    pub fn bid(&self) -> f64 {
        self.bid
    }
}

/// One timestamp's resolved ladder.
#[derive(Debug, Clone, Serialize)]
pub struct ChainSnapshot {
    pub symbol: String,
    pub ts: DateTime<Utc>,
    pub underlying: UnderlyingBar,
    /// Median-of-parity forward across the paired ladder.
    pub forward: f64,
    /// The fit's own confession — roughly triples in the bar an earnings print
    /// lands in, as a few strikes quote stale while market makers reprice.
    pub residual_mad: f64,
    /// Paired strikes the forward was fitted on.
    pub paired_strikes: usize,
    /// Calendar year fraction to the 16:00 ET expiry.
    pub year_fraction: f64,
    pub quotes: Vec<OptionQuote>,
}

impl ChainSnapshot {
    /// Spot where the capture has it, else the parity forward.
    ///
    /// SPY/QQQ/IWM options trade to 20:00 UTC, 15 minutes past the equity close,
    /// so ~7% of their bars have no stock print. The forward is within basis
    /// points of spot bar to bar, which makes it the right fallback — but it is
    /// a fallback, and `underlying.mid` being `None` is how a caller can tell.
    pub fn reference_price(&self) -> f64 {
        self.underlying.price().unwrap_or(self.forward)
    }

    pub fn quote(&self, contract_id: i64) -> Option<&OptionQuote> {
        self.quotes.iter().find(|q| q.contract_id == contract_id)
    }

    /// Quotes of one right, ascending by strike, restricted to those a delta
    /// actually solved for. Deep-ITM strikes quoting at or below intrinsic are
    /// excluded here rather than defaulted downstream.
    pub fn selectable(&self, right: OptionRight) -> impl Iterator<Item = &OptionQuote> {
        self.quotes
            .iter()
            .filter(move |q| q.right == right && q.delta.is_some())
    }

    /// The out-of-the-money half of the vol smile as `(log-moneyness, iv)` points,
    /// sorted ascending. OTM prices are used because deep-ITM quotes are almost
    /// entirely intrinsic and carry no clean vol. `atm_implied_vol` and
    /// `put_call_skew` both read from here, so they agree on what the smile is.
    fn otm_smile(&self) -> Vec<(f64, f64)> {
        let mut smile: Vec<(f64, f64)> = self
            .quotes
            .iter()
            .filter_map(|quote| {
                let is_otm = match quote.right {
                    OptionRight::Call => quote.strike >= self.forward,
                    OptionRight::Put => quote.strike < self.forward,
                };
                is_otm.then(|| {
                    let x = (quote.strike / self.forward).ln();
                    Some((x, quote.implied_vol?))
                })?
            })
            .collect();
        smile.sort_by(|a, b| a.0.total_cmp(&b.0));
        smile
    }

    /// ATM implied volatility, matching `ib-option-viz`: interpolate the OTM smile
    /// at zero log-moneyness.
    pub fn atm_implied_vol(&self) -> Option<f64> {
        interp_smile_iv(&self.otm_smile(), 0.0)
    }

    /// Put/call skew: OTM put IV minus OTM call IV at a symmetric distance from
    /// the money (`moneyness` is a log-moneyness magnitude, e.g. 0.05 for ~5%).
    /// Positive is the usual equity shape — downside protection bid richer than
    /// upside. `None` unless the smile has quotes on **both** wings, since a
    /// one-sided ladder cannot measure a skew (the clamp would compare a wing
    /// against itself and invent a zero).
    pub fn put_call_skew(&self, moneyness: f64) -> Option<f64> {
        let m = moneyness.abs();
        let smile = self.otm_smile();
        let has_put_side = smile.iter().any(|&(x, _)| x < 0.0);
        let has_call_side = smile.iter().any(|&(x, _)| x > 0.0);
        if !has_put_side || !has_call_side {
            return None;
        }
        let put_iv = interp_smile_iv(&smile, -m)?;
        let call_iv = interp_smile_iv(&smile, m)?;
        Some(put_iv - call_iv)
    }
}

/// Linearly interpolate a sorted `(log-moneyness, iv)` smile at `x`, clamping to
/// the nearest quoted end when `x` is outside the quoted range. `None` only when
/// the smile is empty.
fn interp_smile_iv(smile: &[(f64, f64)], x: f64) -> Option<f64> {
    let &(first_x, first_iv) = smile.first()?;
    if x <= first_x {
        return Some(first_iv);
    }
    let &(last_x, last_iv) = smile.last()?;
    if x >= last_x {
        return Some(last_iv);
    }
    smile.windows(2).find_map(|pair| {
        let (lo_x, lo_iv) = pair[0];
        let (hi_x, hi_iv) = pair[1];
        (lo_x <= x && x <= hi_x).then(|| {
            let weight = (x - lo_x) / (hi_x - lo_x);
            lo_iv + weight * (hi_iv - lo_iv)
        })
    })
}

/// A whole symbol/expiry, loaded once.
/// An earnings announcement for one symbol.
#[derive(Debug, Clone)]
pub struct EarningsEvent {
    pub date: NaiveDate,
    /// The last session before the release is priced in — the day a position
    /// that must not hold through it closes. The session *before* the capture's
    /// `reaction_date`, so an after-close release closes the same day and a
    /// Monday pre-open release closes the Friday before.
    pub close_on: NaiveDate,
}

#[derive(Debug)]
pub struct ChainSeries {
    pub symbol: String,
    pub expiry: NaiveDate,
    quotes: Vec<RawQuote>,
    /// Offsets into `quotes` delimiting each timestamp's block.
    blocks: Vec<(DateTime<Utc>, usize, usize)>,
    /// 30-min underlying bars, contemporaneous with option bars. Used in
    /// `snapshot()` so the underlying mid matches the option bar's window.
    underlying: BTreeMap<DateTime<Utc>, UnderlyingBar>,
    /// Fine-grained underlying bars for the signal. 5-min if the snapshot has
    /// them, otherwise the same 30-min data. Only stock-only bars draw from
    /// this; option bars always use `underlying` above.
    signal_underlying: BTreeMap<DateTime<Utc>, UnderlyingBar>,
    expiry_instant: DateTime<Utc>,
    pub option_quality: Quality,
    pub underlying_quality: Quality,
    /// "5min" or "30min" — resolution of `signal_underlying`.
    pub underlying_bar_size: String,
    /// Earnings dates within the backtest window, sorted ascending.
    pub earnings: Vec<EarningsEvent>,
}

impl ChainSeries {
    pub fn load(snapshot: &Snapshot, symbol: &str, expiry: NaiveDate) -> Result<Self> {
        let symbol = normalize_symbol(symbol);
        let expiry_text = expiry.format("%Y-%m-%d").to_string();

        let (quotes, option_quality) = load_option_quotes(snapshot, &symbol, &expiry_text)?;
        let (underlying, underlying_quality) = load_underlying(snapshot, &symbol, BAR_SIZE)?;
        let sessions = underlying.keys().map(|ts| ts.date_naive()).collect();
        let earnings = load_earnings(snapshot, &symbol, &sessions)?;
        let (signal_underlying, signal_quality, underlying_bar_size) = {
            let (u, q) = load_underlying(snapshot, &symbol, "5min")?;
            if !u.is_empty() {
                (u, q, "5min".to_string())
            } else {
                (underlying.clone(), underlying_quality, BAR_SIZE.to_string())
            }
        };

        let mut blocks = Vec::new();
        let mut start = 0usize;
        while start < quotes.len() {
            let ts = quotes[start].ts;
            let mut end = start;
            while end < quotes.len() && quotes[end].ts == ts {
                end += 1;
            }
            blocks.push((ts, start, end));
            start = end;
        }

        Ok(Self {
            symbol,
            expiry,
            quotes,
            blocks,
            underlying,
            signal_underlying,
            expiry_instant: pricing::expiry_instant(expiry)?,
            option_quality,
            underlying_quality: signal_quality,
            underlying_bar_size,
            earnings,
        })
    }

    /// Every timestamp with at least one clean two-sided option quote, ascending.
    pub fn timestamps(&self) -> impl ExactSizeIterator<Item = DateTime<Utc>> + '_ {
        self.blocks.iter().map(|&(ts, _, _)| ts)
    }

    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// The 30-min underlying at any timestamp.
    pub fn underlying_at(&self, ts: DateTime<Utc>) -> Option<UnderlyingBar> {
        self.underlying.get(&ts).copied()
    }

    /// The last underlying print at or before `ts`, checking the fine-grained
    /// timeline first (5-min bars run later in the day than 30-min on ETFs).
    pub fn underlying_asof(&self, ts: DateTime<Utc>) -> Option<UnderlyingBar> {
        self.signal_underlying
            .range(..=ts)
            .next_back()
            .or_else(|| self.underlying.range(..=ts).next_back())
            .map(|(_, v)| *v)
    }

    pub fn expiry_instant(&self) -> DateTime<Utc> {
        self.expiry_instant
    }

    /// The next earnings event on or after the given date. The engine uses this
    /// to close positions before earnings and to refuse entries in the blackout.
    pub fn next_earnings_from(&self, date: NaiveDate) -> Option<&EarningsEvent> {
        self.earnings.iter().find(|e| e.date >= date)
    }

    /// The most recent earnings event on or before the given date — the other
    /// half of an earnings-distance feature. `None` before the first event in the
    /// window. `earnings` is sorted ascending, so scan from the back.
    pub fn previous_earnings_from(&self, date: NaiveDate) -> Option<&EarningsEvent> {
        self.earnings.iter().rev().find(|e| e.date <= date)
    }

    /// All timestamps where either option or signal-underlying data exists,
    /// ascending. Each entry carries the option bar index if one exists at
    /// that timestamp. When the signal underlying is 5-min, this interleaves
    /// ~5 stock-only bars between each pair of 30-min option bars.
    pub fn merged_timeline(&self) -> Vec<(DateTime<Utc>, Option<usize>)> {
        let mut option_set: std::collections::HashSet<DateTime<Utc>> =
            std::collections::HashSet::with_capacity(self.blocks.len());
        let mut out: Vec<(DateTime<Utc>, Option<usize>)> = Vec::new();

        for (idx, &(ts, _, _)) in self.blocks.iter().enumerate() {
            option_set.insert(ts);
            out.push((ts, Some(idx)));
        }
        for &ts in self.signal_underlying.keys() {
            if !option_set.contains(&ts) {
                out.push((ts, None));
            }
        }
        out.sort_by_key(|&(ts, _)| ts);
        out
    }

    /// A minimal snapshot for stock-only bars where no option chain exists.
    /// Uses the fine-grained signal underlying (5-min). `atm_implied_vol()`
    /// returns None and no quotes are selectable.
    pub fn stock_only_snapshot(&self, ts: DateTime<Utc>) -> Option<ChainSnapshot> {
        let underlying = self.signal_underlying.get(&ts).copied()?;
        let price = underlying.price()?;
        Some(ChainSnapshot {
            symbol: self.symbol.clone(),
            ts,
            underlying,
            forward: price,
            residual_mad: 0.0,
            paired_strikes: 0,
            year_fraction: pricing::year_fraction(ts, self.expiry_instant),
            quotes: Vec::new(),
        })
    }

    /// Resolve the ladder at block `index`: fit the forward, invert for vol,
    /// derive delta. One pass, no allocation beyond the returned quotes.
    pub fn snapshot(&self, index: usize) -> Option<ChainSnapshot> {
        let &(ts, start, end) = self.blocks.get(index)?;
        let raw = &self.quotes[start..end];

        // Pair calls with puts by strike for the parity fit. The ladder is small
        // (30-49 strikes) and already sorted by strike, so a BTreeMap here costs
        // nothing and keeps the pairing obvious.
        let mut by_strike: BTreeMap<u64, (Option<f64>, Option<f64>)> = BTreeMap::new();
        for q in raw {
            let entry = by_strike.entry(q.strike.to_bits()).or_default();
            let mid = 0.5 * (q.bid + q.ask);
            match q.right {
                OptionRight::Call => entry.0 = Some(mid),
                OptionRight::Put => entry.1 = Some(mid),
            }
        }
        let ladder: Vec<PairedStrike> = by_strike
            .iter()
            .filter_map(|(bits, &(call, put))| {
                Some(PairedStrike {
                    strike: f64::from_bits(*bits),
                    call_mid: call?,
                    put_mid: put?,
                })
            })
            .collect();

        let fit = pricing::parity_fit(&ladder)?;
        let year_fraction = pricing::year_fraction(ts, self.expiry_instant);

        let quotes = raw
            .iter()
            .map(|q| {
                let mid = 0.5 * (q.bid + q.ask);
                let is_call = q.right.is_call();
                let implied_vol =
                    pricing::implied_vol(mid, fit.forward, q.strike, year_fraction, is_call);
                let delta = implied_vol
                    .and_then(|v| pricing::delta(fit.forward, q.strike, year_fraction, v, is_call));
                OptionQuote {
                    contract_id: q.contract_id,
                    strike: q.strike,
                    right: q.right,
                    bid: q.bid,
                    ask: q.ask,
                    mid,
                    implied_vol,
                    delta,
                }
            })
            .collect();

        Some(ChainSnapshot {
            symbol: self.symbol.clone(),
            ts,
            underlying: self.underlying_at(ts).unwrap_or_default(),
            forward: fit.forward,
            residual_mad: fit.residual_mad,
            paired_strikes: ladder.len(),
            year_fraction,
            quotes,
        })
    }

    /// Resolve the ladder at a specific timestamp.
    pub fn snapshot_at(&self, ts: DateTime<Utc>) -> Option<ChainSnapshot> {
        let index = self.blocks.binary_search_by_key(&ts, |&(t, _, _)| t).ok()?;
        self.snapshot(index)
    }
}

/// The pivot is what makes unpaired quotes countable: an inner join would
/// silently return only the rows that already have both sides.
const OPTION_SQL: &str = r#"
SELECT b.ts, c.id AS contract_id, c.strike, c.right,
       MAX(CASE WHEN b.data_type = 'bid' THEN b.close END) AS bid,
       MAX(CASE WHEN b.data_type = 'ask' THEN b.close END) AS ask
FROM bars b
JOIN contracts c ON c.id = b.contract_id
WHERE b.bar_size = ?1
  AND b.data_type IN ('bid', 'ask')
  AND c.kind   = 'option'
  AND c.symbol = ?2
  AND c.expiry = ?3
GROUP BY b.ts, c.id
ORDER BY b.ts, c.strike, c.right
"#;

const UNDERLYING_SQL: &str = r#"
SELECT b.ts,
       MAX(CASE WHEN b.data_type = 'bid'    THEN b.close END) AS bid,
       MAX(CASE WHEN b.data_type = 'ask'    THEN b.close END) AS ask,
       MAX(CASE WHEN b.data_type = 'trades' THEN b.open END) AS open,
       MAX(CASE WHEN b.data_type = 'trades' THEN b.high END) AS high,
       MAX(CASE WHEN b.data_type = 'trades' THEN b.low END) AS low,
       MAX(CASE WHEN b.data_type = 'trades' THEN b.close END) AS close,
       MAX(CASE WHEN b.data_type = 'trades' THEN b.volume END) AS volume
FROM bars b
JOIN contracts c ON c.id = b.contract_id
WHERE b.bar_size = ?1
  AND c.kind   = 'stock'
  AND c.symbol = ?2
GROUP BY b.ts
ORDER BY b.ts
"#;

fn load_option_quotes(
    snapshot: &Snapshot,
    symbol: &str,
    expiry: &str,
) -> Result<(Vec<RawQuote>, Quality)> {
    let mut stmt = snapshot.conn().prepare(OPTION_SQL)?;
    let rows = stmt.query_map((BAR_SIZE, symbol, expiry), |row| {
        Ok((
            row.get::<_, String>("ts")?,
            row.get::<_, i64>("contract_id")?,
            row.get::<_, f64>("strike")?,
            row.get::<_, String>("right")?,
            row.get::<_, Option<f64>>("bid")?,
            row.get::<_, Option<f64>>("ask")?,
        ))
    })?;

    let mut quality = Quality::default();
    let mut quotes = Vec::new();
    for row in rows {
        let (ts, contract_id, strike, right, bid, ask) = row?;
        quality.rows_read += 1;
        let (Some(bid), Some(ask)) = (bid, ask) else {
            quality.dropped_unpaired += 1;
            continue;
        };
        if bid > ask {
            quality.dropped_crossed += 1;
            continue;
        }
        if 0.5 * (bid + ask) <= 0.0 {
            quality.dropped_nonpositive += 1;
            continue;
        }
        quotes.push(RawQuote {
            ts: parse_ts(&ts)?,
            contract_id,
            strike,
            right: right.parse()?,
            bid,
            ask,
        });
    }
    Ok((quotes, quality))
}

fn load_underlying(
    snapshot: &Snapshot,
    symbol: &str,
    bar_size: &str,
) -> Result<(BTreeMap<DateTime<Utc>, UnderlyingBar>, Quality)> {
    let mut stmt = snapshot.conn().prepare(UNDERLYING_SQL)?;
    let rows = stmt.query_map((bar_size, symbol), |row| {
        Ok((
            row.get::<_, String>("ts")?,
            row.get::<_, Option<f64>>("bid")?,
            row.get::<_, Option<f64>>("ask")?,
            row.get::<_, Option<f64>>("open")?,
            row.get::<_, Option<f64>>("high")?,
            row.get::<_, Option<f64>>("low")?,
            row.get::<_, Option<f64>>("close")?,
            row.get::<_, Option<f64>>("volume")?,
        ))
    })?;

    let mut quality = Quality::default();
    let mut out = BTreeMap::new();
    for row in rows {
        let (ts, bid, ask, open, high, low, close, volume) = row?;
        quality.rows_read += 1;
        // A missing side costs the mid, not the whole bar: the trades close still
        // drives indicators, so the bar is kept and only `mid` goes None.
        let mid = match (bid, ask) {
            (Some(bid), Some(ask)) if bid > ask => {
                quality.dropped_crossed += 1;
                None
            }
            (Some(bid), Some(ask)) if bid + ask <= 0.0 => {
                quality.dropped_nonpositive += 1;
                None
            }
            (Some(bid), Some(ask)) => Some(0.5 * (bid + ask)),
            _ => {
                quality.dropped_unpaired += 1;
                None
            }
        };
        out.insert(
            parse_ts(&ts)?,
            UnderlyingBar {
                mid,
                open,
                high,
                low,
                close,
                volume,
            },
        );
    }
    Ok((out, quality))
}

/// `reaction_date` is the session the capture says absorbs the release; it
/// already encodes before-open against after-close, so no timing rule is
/// repeated here.
fn load_earnings(
    snapshot: &Snapshot,
    symbol: &str,
    sessions: &BTreeSet<NaiveDate>,
) -> Result<Vec<EarningsEvent>> {
    let mut stmt = snapshot.conn().prepare(
        "SELECT event_date,
                COALESCE(json_extract(detail_json, '$.reaction_date'), event_date)
         FROM events
         WHERE symbol = ?1 AND kind = 'earnings'
         ORDER BY event_date",
    )?;
    let rows = stmt.query_map([symbol], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let parse = |text: &str| {
        NaiveDate::parse_from_str(text, "%Y-%m-%d")
            .with_context(|| format!("earnings date '{text}'"))
    };
    let mut out = Vec::new();
    for row in rows {
        let (date, reaction) = row?;
        out.push(EarningsEvent {
            date: parse(&date)?,
            close_on: session_before(parse(&reaction)?, sessions),
        });
    }
    Ok(out)
}

/// The last session before `day`. Read off the stock tape where the tape spans
/// `day`, so a holiday is skipped like a weekend; otherwise — the release is past
/// the end of the data — the previous weekday, rather than the tape's last day,
/// which would close a position for an event still a week away.
fn session_before(day: NaiveDate, sessions: &BTreeSet<NaiveDate>) -> NaiveDate {
    match sessions.range(..day).next_back() {
        Some(&session) if sessions.range(day..).next().is_some() => session,
        _ => {
            let mut prior = day.pred_opt().expect("a capture date has a predecessor");
            while prior.weekday().number_from_monday() > 5 {
                prior = prior.pred_opt().expect("a capture date has a predecessor");
            }
            prior
        }
    }
}

/// Every writer emits RFC3339 UTC with a literal `+00:00`; lexicographic range
/// queries elsewhere depend on that exact form, and so does this parse.
fn parse_ts(text: &str) -> Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(text)
        .with_context(|| format!("timestamp '{text}' is not RFC3339"))?
        .with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quality_accounts_for_every_row_it_read() {
        let q = Quality {
            rows_read: 100,
            dropped_unpaired: 3,
            dropped_crossed: 2,
            dropped_nonpositive: 1,
        };
        assert_eq!(q.rows_kept(), 94);
    }

    /// The before-earnings exit closes on this date, so a miss here holds a
    /// position through the print. 33 of 254 pre-open releases in the reference
    /// snapshot land on a Monday.
    #[test]
    fn the_session_before_a_release_skips_weekends_and_holidays() {
        let d = |m, day| NaiveDate::from_ymd_opt(2026, m, day).unwrap();
        // 2026-09-07 is Labor Day; the tape trades Thu 3, Fri 4, Tue 8, Wed 9.
        let tape: BTreeSet<NaiveDate> = [d(9, 3), d(9, 4), d(9, 8), d(9, 9)].into();

        assert_eq!(session_before(d(9, 9), &tape), d(9, 8), "after-close Tuesday");
        assert_eq!(session_before(d(9, 8), &tape), d(9, 4), "pre-open after the holiday");
        // Past the tape: the previous weekday, never the tape's last session.
        assert_eq!(session_before(d(9, 21), &tape), d(9, 18), "a Monday past the data");
        assert_eq!(session_before(d(9, 16), &tape), d(9, 15));
    }

    #[test]
    fn timestamps_parse_the_capture_format() {
        let ts = parse_ts("2026-08-19T15:00:00+00:00").unwrap();
        assert_eq!(ts.to_rfc3339(), "2026-08-19T15:00:00+00:00");
        assert!(parse_ts("2026-08-19 15:00:00").is_err());
    }

    /// Build a minimal but real snapshot on disk: `ChainSeries::load` runs actual
    /// SQL, so a mock would test nothing. Two strikes, two timestamps, plus the
    /// deliberate defects the Quality counters exist to catch.
    fn fixture(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("ib-spread-trader-chain-fixtures");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{name}.sqlite3"));
        let _ = std::fs::remove_file(&path);

        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.pragma_update(None, "user_version", 6i64).unwrap();
        conn.execute_batch(
            "CREATE TABLE contracts (id INTEGER PRIMARY KEY, symbol TEXT, kind TEXT,
                 exchange TEXT, currency TEXT, expiry TEXT, strike REAL, right TEXT);
             CREATE TABLE bars (contract_id INTEGER, ts TEXT, bar_size TEXT, data_type TEXT,
                 open REAL, high REAL, low REAL, close REAL, volume REAL);
             CREATE TABLE events (id INTEGER PRIMARY KEY, symbol TEXT,
                 event_date TEXT NOT NULL, event_time TEXT, session_timing TEXT,
                 kind TEXT NOT NULL, source TEXT NOT NULL, source_id TEXT, title TEXT, detail_json TEXT);
             INSERT INTO contracts (id, symbol, kind, exchange, currency, expiry, strike, right)
             VALUES (1, 'TEST', 'stock',  'SMART', 'USD', NULL,         NULL, NULL),
                    (2, 'TEST', 'option', 'SMART', 'USD', '2026-08-21', 100.0, 'C'),
                    (3, 'TEST', 'option', 'SMART', 'USD', '2026-08-21', 100.0, 'P'),
                    (4, 'TEST', 'option', 'SMART', 'USD', '2026-08-21', 105.0, 'C'),
                    (5, 'TEST', 'option', 'SMART', 'USD', '2026-08-21', 105.0, 'P');",
        )
        .unwrap();

        // Insertion order is irrelevant — the loader's SQL orders by ts and strike.
        let bars: Vec<(i64, &str, &str, f64)> = vec![
            // Bar one: a complete, clean ladder.
            (2, "2026-07-01T13:30:00+00:00", "bid", 4.90),
            (2, "2026-07-01T13:30:00+00:00", "ask", 5.10),
            (3, "2026-07-01T13:30:00+00:00", "bid", 4.90),
            (3, "2026-07-01T13:30:00+00:00", "ask", 5.10),
            (4, "2026-07-01T13:30:00+00:00", "bid", 2.90),
            (4, "2026-07-01T13:30:00+00:00", "ask", 3.10),
            (5, "2026-07-01T13:30:00+00:00", "bid", 7.90),
            (5, "2026-07-01T13:30:00+00:00", "ask", 8.10),
            // Bar two: strike 100 is clean; 105C prints only a bid (unpaired) and
            // 105P prints crossed. Both must be dropped and counted.
            (2, "2026-07-01T14:00:00+00:00", "bid", 5.40),
            (2, "2026-07-01T14:00:00+00:00", "ask", 5.60),
            (3, "2026-07-01T14:00:00+00:00", "bid", 4.40),
            (3, "2026-07-01T14:00:00+00:00", "ask", 4.60),
            (4, "2026-07-01T14:00:00+00:00", "bid", 3.30),
            (5, "2026-07-01T14:00:00+00:00", "bid", 8.10),
            (5, "2026-07-01T14:00:00+00:00", "ask", 7.90),
            // The underlying, including one print past the last option bar —
            // what expiry settlement reads.
            (1, "2026-07-01T13:30:00+00:00", "trades", 100.00),
            (1, "2026-07-01T13:30:00+00:00", "bid", 99.99),
            (1, "2026-07-01T13:30:00+00:00", "ask", 100.01),
            (1, "2026-07-01T14:00:00+00:00", "trades", 100.50),
            (1, "2026-07-01T14:00:00+00:00", "bid", 100.49),
            (1, "2026-07-01T14:00:00+00:00", "ask", 100.51),
            (1, "2026-08-21T19:30:00+00:00", "trades", 108.00),
        ];
        for (id, ts, kind, close) in bars {
            conn.execute(
                "INSERT INTO bars (contract_id, ts, bar_size, data_type, open, high, low, close, volume)
                 VALUES (?1, ?2, '30min', ?3, ?4, ?4, ?4, ?4, 0)",
                rusqlite::params![id, ts, kind, close],
            )
            .unwrap();
        }
        drop(conn);
        path
    }

    fn expiry() -> NaiveDate {
        NaiveDate::from_ymd_opt(2026, 8, 21).unwrap()
    }

    #[test]
    fn loads_a_chain_and_resolves_its_ladder() {
        let path = fixture("basic");
        let snap = Snapshot::open(&path).unwrap();
        let series = ChainSeries::load(&snap, "test", expiry()).unwrap();

        assert_eq!(series.symbol, "TEST", "symbols normalize on load");
        assert_eq!(series.len(), 2);

        let first = series.snapshot(0).unwrap();
        assert_eq!(first.paired_strikes, 2);
        assert_eq!(first.quotes.len(), 4);
        // K + C - P is 100 + 5 - 5 = 100 at one strike and 105 + 3 - 8 = 100 at
        // the other, so the median forward is exactly 100 with no residual.
        assert!((first.forward - 100.0).abs() < 1e-12);
        assert_eq!(first.residual_mad, 0.0);
        assert_eq!(first.underlying.open, Some(100.0));
        assert_eq!(first.underlying.high, Some(100.0));
        assert_eq!(first.underlying.low, Some(100.0));
        assert_eq!(first.underlying.close, Some(100.0));
        assert_eq!(first.underlying.volume, Some(0.0));
        assert!((first.underlying.mid.unwrap() - 100.0).abs() < 1e-12);
    }

    #[test]
    fn atm_iv_interpolates_only_the_otm_smile() {
        let quote = |strike: f64, right: OptionRight, iv: f64| OptionQuote {
            contract_id: (strike as i64) * 10 + i64::from(right.is_call()),
            strike,
            right,
            bid: 1.0,
            ask: 1.2,
            mid: 1.1,
            implied_vol: Some(iv),
            delta: Some(0.5),
        };
        let bar = ChainSnapshot {
            symbol: "TEST".into(),
            ts: Utc::now(),
            underlying: UnderlyingBar::default(),
            forward: 100.0,
            residual_mad: 0.0,
            paired_strikes: 4,
            year_fraction: 0.1,
            quotes: vec![
                quote(90.0, OptionRight::Put, 0.30),
                quote(99.0, OptionRight::Put, 0.20),
                quote(101.0, OptionRight::Call, 0.22),
                quote(110.0, OptionRight::Call, 0.32),
                // These are ITM and deliberately absurd: neither may affect ATM.
                quote(99.0, OptionRight::Call, 0.90),
                quote(101.0, OptionRight::Put, 0.90),
            ],
        };

        let lo = (99.0_f64 / 100.0).ln();
        let hi = (101.0_f64 / 100.0).ln();
        let expected = 0.20 + (-lo / (hi - lo)) * (0.22 - 0.20);
        assert!((bar.atm_implied_vol().unwrap() - expected).abs() < 1e-12);
    }

    #[test]
    fn put_call_skew_reads_the_two_wings() {
        let quote = |strike: f64, right: OptionRight, iv: f64| OptionQuote {
            contract_id: (strike as i64) * 10 + i64::from(right.is_call()),
            strike,
            right,
            bid: 1.0,
            ask: 1.2,
            mid: 1.1,
            implied_vol: Some(iv),
            delta: Some(0.5),
        };
        let snap = |quotes| ChainSnapshot {
            symbol: "TEST".into(),
            ts: Utc::now(),
            underlying: UnderlyingBar::default(),
            forward: 100.0,
            residual_mad: 0.0,
            paired_strikes: 4,
            year_fraction: 0.1,
            quotes,
        };

        // Puts bid richer than calls: a positive equity skew of roughly the raw
        // 0.10 wing difference.
        let skewed = snap(vec![
            quote(90.0, OptionRight::Put, 0.35),
            quote(95.0, OptionRight::Put, 0.30),
            quote(105.0, OptionRight::Call, 0.20),
            quote(110.0, OptionRight::Call, 0.18),
        ]);
        let skew = skewed.put_call_skew(0.05).unwrap();
        assert!((0.08..=0.12).contains(&skew), "{skew}");

        // A flat smile is (near) zero skew.
        let flat = snap(vec![
            quote(95.0, OptionRight::Put, 0.25),
            quote(105.0, OptionRight::Call, 0.25),
        ]);
        assert!(flat.put_call_skew(0.05).unwrap().abs() < 1e-9);

        // A ladder with only one wing quoted cannot measure a skew.
        let one_sided = snap(vec![
            quote(105.0, OptionRight::Call, 0.20),
            quote(110.0, OptionRight::Call, 0.18),
        ]);
        assert_eq!(one_sided.put_call_skew(0.05), None);
    }

    #[test]
    fn defective_quotes_are_dropped_and_counted() {
        let path = fixture("defects");
        let snap = Snapshot::open(&path).unwrap();
        let series = ChainSeries::load(&snap, "TEST", expiry()).unwrap();

        // 8 contract-timestamps printed something (4 strikes x 2 bars); of the
        // second bar's four, 105C printed one side only and 105P printed
        // crossed, so 6 survive.
        assert_eq!(series.option_quality.rows_read, 8);
        assert_eq!(series.option_quality.dropped_unpaired, 1);
        assert_eq!(series.option_quality.dropped_crossed, 1);
        assert_eq!(series.option_quality.dropped_nonpositive, 0);
        assert_eq!(series.option_quality.rows_kept(), 6);

        // The second bar keeps only strike 100 — and the fit falls back to it
        // rather than reaching for the dropped strike.
        let second = series.snapshot(1).unwrap();
        assert_eq!(second.quotes.len(), 2);
        assert_eq!(second.paired_strikes, 1);
        assert!((second.forward - 101.0).abs() < 1e-12, "{}", second.forward);
    }

    /// The option tape stops before expiry, so settlement has to read the stock
    /// print that lands after the last option bar.
    #[test]
    fn the_underlying_is_readable_past_the_last_option_bar() {
        let path = fixture("settlement");
        let snap = Snapshot::open(&path).unwrap();
        let series = ChainSeries::load(&snap, "TEST", expiry()).unwrap();

        let last_option_bar = series.timestamps().last().unwrap();
        assert_eq!(last_option_bar.to_rfc3339(), "2026-07-01T14:00:00+00:00");

        let at_expiry = series.underlying_asof(series.expiry_instant()).unwrap();
        assert_eq!(at_expiry.close, Some(108.0));
    }

    /// Nothing here has a spot, so `reference_price` must fall through to the
    /// forward — the case SPY/QQQ/IWM hit on every 20:00 UTC bar.
    #[test]
    fn a_bar_without_a_stock_print_falls_back_to_the_forward() {
        let path = fixture("nospot");
        let snap = Snapshot::open(&path).unwrap();
        let series = ChainSeries::load(&snap, "TEST", expiry()).unwrap();

        let mut bar = series.snapshot(0).unwrap();
        bar.underlying = UnderlyingBar {
            ..UnderlyingBar::default()
        };
        assert_eq!(bar.reference_price(), bar.forward);
    }

    #[test]
    fn underlying_prefers_the_trades_close_and_falls_back_to_the_mid() {
        assert_eq!(
            UnderlyingBar {
                mid: Some(2.0),
                close: Some(1.0),
                ..UnderlyingBar::default()
            }
            .price(),
            Some(1.0)
        );
        assert_eq!(
            UnderlyingBar {
                mid: Some(2.0),
                ..UnderlyingBar::default()
            }
            .price(),
            Some(2.0)
        );
        assert_eq!(UnderlyingBar::default().price(), None);
    }
}
