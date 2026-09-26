//! What is actually backtestable in a snapshot.
//!
//! The distinction this module exists to enforce: **a registered contract is not
//! tradeable data.** The reference snapshot holds 4 280 option contracts but only
//! 886 of them ever received a bar, and two of its three expiries have none at
//! all. Anything that plans a backtest by reading `contracts` will silently
//! promise chains it cannot price.
//!
//! So coverage is measured from `bars`, and specifically from bars where **both
//! sides of the market printed at the same timestamp** — a lone bid cannot be
//! filled against.

use anyhow::Result;
use serde::Serialize;

use super::snapshot::Snapshot;

/// The option bar sizes this crate trades on, finest first. Captures from
/// 2026-09 on take 15-minute option bars; the reference snapshot is 30-minute.
pub const OPTION_BAR_SIZES: [&str; 2] = ["15min", "30min"];

/// The stock bars an option bar size is checked against for `stock_ts_overlap`:
/// its own size where the capture took it, the 5-minute tape for 15-minute
/// chains, which were captured without 15-minute stock bars.
const STOCK_BAR_SIZE: [(&str, &str); 2] = [("15min", "5min"), ("30min", "30min")];

#[derive(Debug, Clone, Serialize)]
pub struct ChainCoverage {
    pub symbol: String,
    pub expiry: String,
    /// The option bar size this row counts. A chain captured at both sizes is
    /// listed once per size; the backtester trades the finest.
    pub bar_size: String,
    /// Strikes with a two-sided quote at some timestamp.
    pub strikes: i64,
    /// Strikes quoted on **both** call and put sides — what the parity fit needs.
    pub paired_strikes: i64,
    pub timestamps: i64,
    pub sessions: i64,
    pub first_ts: String,
    pub last_ts: String,
    /// Two-sided option bars in total.
    pub quote_bars: i64,
    /// Option timestamps that also have an underlying bar. Short of `timestamps`
    /// for SPY/QQQ/IWM, whose options trade 15 minutes past the equity close.
    pub stock_ts_overlap: i64,
    /// Earnings releases whose reaction session falls inside the chain's window.
    pub earnings_in_window: i64,
}

impl ChainCoverage {
    /// Whether this chain has enough of a ladder to fit a forward and select strikes.
    pub fn is_tradeable(&self) -> bool {
        self.paired_strikes >= 4 && self.sessions >= 2
    }
}

/// Coverage for every symbol/expiry that has at least one two-sided option bar.
pub fn chain_coverage(snapshot: &Snapshot) -> Result<Vec<ChainCoverage>> {
    let mut chains = Vec::new();
    for (option_size, stock_size) in STOCK_BAR_SIZE {
        chains.extend(coverage_at(snapshot, option_size, stock_size)?);
    }
    chains.sort_by(|a, b| (&a.symbol, &a.expiry, &a.bar_size).cmp(&(&b.symbol, &b.expiry, &b.bar_size)));
    Ok(chains)
}

fn coverage_at(snapshot: &Snapshot, option_size: &str, stock_size: &str) -> Result<Vec<ChainCoverage>> {
    let mut stmt = snapshot.conn().prepare(SQL)?;
    let rows = stmt.query_map([option_size, stock_size], |row| {
        Ok(ChainCoverage {
            symbol: row.get("symbol")?,
            expiry: row.get("expiry")?,
            bar_size: option_size.to_string(),
            strikes: row.get("strikes")?,
            paired_strikes: row.get("paired_strikes")?,
            timestamps: row.get("timestamps")?,
            sessions: row.get("sessions")?,
            first_ts: row.get("first_ts")?,
            last_ts: row.get("last_ts")?,
            quote_bars: row.get("quote_bars")?,
            stock_ts_overlap: row.get("stock_ts_overlap")?,
            earnings_in_window: row.get("earnings_in_window")?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// `quote` is the spine: one row per (contract, ts) that printed a bid *and* an
/// ask. Every count below is derived from it, so no aggregate can credit a chain
/// for a one-sided quote.
const SQL: &str = r#"
WITH quote AS (
    SELECT c.symbol, c.expiry, c.strike, c.right, bid.ts
    FROM bars bid
    JOIN bars ask
      ON ask.contract_id = bid.contract_id
     AND ask.ts          = bid.ts
     AND ask.bar_size    = bid.bar_size
     AND ask.data_type   = 'ask'
    JOIN contracts c ON c.id = bid.contract_id
    WHERE bid.data_type = 'bid'
      AND bid.bar_size  = ?1
      AND c.kind        = 'option'
),
per_chain AS (
    SELECT symbol, expiry,
           COUNT(DISTINCT strike)          AS strikes,
           COUNT(DISTINCT ts)              AS timestamps,
           COUNT(DISTINCT substr(ts, 1, 10)) AS sessions,
           MIN(ts)                         AS first_ts,
           MAX(ts)                         AS last_ts,
           COUNT(*)                        AS quote_bars
    FROM quote
    GROUP BY symbol, expiry
),
paired AS (
    SELECT symbol, expiry, COUNT(*) AS paired_strikes
    FROM (
        SELECT symbol, expiry, strike
        FROM quote
        GROUP BY symbol, expiry, strike
        HAVING COUNT(DISTINCT right) = 2
    )
    GROUP BY symbol, expiry
),
stock AS (
    SELECT c.symbol, b.ts
    FROM bars b
    JOIN contracts c ON c.id = b.contract_id
    WHERE c.kind = 'stock' AND b.bar_size = ?2 AND b.data_type = 'trades'
),
overlap AS (
    SELECT q.symbol, q.expiry, COUNT(*) AS stock_ts_overlap
    FROM (SELECT DISTINCT symbol, expiry, ts FROM quote) q
    JOIN stock s ON s.symbol = q.symbol AND s.ts = q.ts
    GROUP BY q.symbol, q.expiry
)
SELECT p.symbol, p.expiry, p.strikes,
       COALESCE(pr.paired_strikes, 0) AS paired_strikes,
       p.timestamps, p.sessions, p.first_ts, p.last_ts, p.quote_bars,
       COALESCE(o.stock_ts_overlap, 0) AS stock_ts_overlap,
       (SELECT COUNT(*) FROM events e
         WHERE e.symbol = p.symbol
           AND e.kind   = 'earnings'
           AND COALESCE(json_extract(e.detail_json, '$.reaction_date'), e.event_date)
               BETWEEN substr(p.first_ts, 1, 10) AND substr(p.last_ts, 1, 10)
       ) AS earnings_in_window
FROM per_chain p
LEFT JOIN paired  pr ON pr.symbol = p.symbol AND pr.expiry = p.expiry
LEFT JOIN overlap o  ON o.symbol  = p.symbol AND o.expiry  = p.expiry
ORDER BY p.symbol, p.expiry
"#;
