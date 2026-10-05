//! LLM headline scores from ib-option-calc's `news-score` cache
//! (`news-llm.sqlite3`, fetched beside the snapshots by
//! `scripts/fetch_news_cache.sh`), reduced to the decayed sentiment the entry
//! gate reads. The same reduction as `scripts/news_scores.py`'s
//! `sentiment_index` and the viz's Sentiment pane:
//!
//! - Only the newest model and prompt version, so a re-score never mixes with
//!   the run it replaces.
//! - Each non-repeat effect of materiality >= 1 on a symbol — its own headline
//!   or another symbol's read through — counts `materiality × sentiment` from
//!   its window's end, the instant the score existed, never the headline's own
//!   time.
//! - Scored windows form **segments** (no gap over four days). Outside one, or
//!   before it has run three half-lives, the sentiment is unknown — `None`,
//!   not zero, so a gate refuses nothing it cannot read.
//!
//! Opened read-only like a snapshot, though unlike one it can be regenerated.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;

/// The cache's file name, beside the snapshots.
pub const CACHE_NAME: &str = "news-llm.sqlite3";
const SEGMENT_GAP_DAYS: i64 = 4;
const WARM_HALF_LIVES: i32 = 3;

#[derive(Debug, Default)]
pub struct NewsSentiment {
    /// Per symbol, (available at, materiality × sentiment), time-sorted.
    effects: HashMap<String, Vec<(DateTime<Utc>, f64)>>,
    segments: Vec<(DateTime<Utc>, DateTime<Utc>)>,
}

#[derive(Deserialize)]
struct Answer {
    items: Vec<Item>,
}

#[derive(Deserialize)]
struct Item {
    id: String,
    is_repeat: bool,
    effects: Vec<Effect>,
}

#[derive(Deserialize)]
struct Effect {
    symbol: String,
    materiality: u8,
    sentiment: f64,
}

impl NewsSentiment {
    pub fn load(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let uri = format!(
            "file:{}?mode=ro",
            path.canonicalize()
                .with_context(|| format!("no news cache at {}", path.display()))?
                .display()
        );
        let conn = Connection::open_with_flags(
            &uri,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
        )
        .with_context(|| format!("opening news cache {}", path.display()))?;
        let (model, version): (String, String) = conn
            .query_row(
                "SELECT model, prompt_version FROM calls ORDER BY created_at DESC LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .context("news cache has no calls")?;
        let mut stmt = conn.prepare(
            "SELECT window_end, items_json, raw_json FROM calls WHERE model = ?1 AND prompt_version = ?2",
        )?;
        let mut calls = Vec::new();
        for row in stmt.query_map([&model, &version], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })? {
            let (end, items, raw) = row?;
            let end = DateTime::parse_from_rfc3339(&end)
                .with_context(|| format!("window_end {end}"))?
                .with_timezone(&Utc);
            let sent: Vec<serde_json::Value> = serde_json::from_str(&items)?;
            let answer: Answer =
                serde_json::from_str(&raw).with_context(|| format!("answer for window {end}"))?;
            calls.push((end, sent.len(), answer));
        }
        Ok(Self::from_calls(calls))
    }

    /// Built from (window end, items sent, answer). An answer whose id matches
    /// no item sent is dropped, as the Python readers drop it.
    fn from_calls(calls: Vec<(DateTime<Utc>, usize, Answer)>) -> Self {
        let mut out = Self::default();
        let mut ends = Vec::new();
        for (end, sent, answer) in calls {
            for item in answer.items {
                if item.id.parse::<usize>().map_or(true, |i| i >= sent) {
                    continue;
                }
                ends.push(end);
                if item.is_repeat {
                    continue;
                }
                for e in item.effects.into_iter().filter(|e| e.materiality >= 1) {
                    out.effects
                        .entry(e.symbol)
                        .or_default()
                        .push((end, f64::from(e.materiality) * e.sentiment));
                }
            }
        }
        for v in out.effects.values_mut() {
            v.sort_by_key(|(at, _)| *at);
        }
        ends.sort();
        ends.dedup();
        let mut start = 0;
        for i in 1..=ends.len() {
            if i == ends.len() || ends[i] - ends[i - 1] > Duration::days(SEGMENT_GAP_DAYS) {
                out.segments.push((ends[start], ends[i - 1]));
                start = i;
            }
        }
        out
    }

    /// The decayed sentiment on `symbol` as of `t`: every effect available by
    /// then, weighted by 2^(−age / half_life). None when `t` is outside a scored
    /// segment or before the segment has run three half-lives. ponytail: decays
    /// in calendar time, so a weekend fades news as much as two sessions do.
    pub fn at(&self, symbol: &str, t: DateTime<Utc>, half_life: Duration) -> Option<f64> {
        let warm = half_life * WARM_HALF_LIVES;
        if !self.segments.iter().any(|&(a, b)| a + warm <= t && t <= b) {
            return None;
        }
        let h = half_life.num_seconds() as f64;
        Some(
            self.effects
                .get(symbol)
                .into_iter()
                .flatten()
                .take_while(|(at, _)| *at <= t)
                .map(|(at, v)| v * 0.5_f64.powf((t - *at).num_seconds() as f64 / h))
                .sum(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn call(
        end: DateTime<Utc>,
        items: Vec<(&str, bool, Vec<(&str, u8, f64)>)>,
    ) -> (DateTime<Utc>, usize, Answer) {
        let n = items.len();
        let items = items
            .into_iter()
            .map(|(id, is_repeat, effects)| Item {
                id: id.into(),
                is_repeat,
                effects: effects
                    .into_iter()
                    .map(|(s, m, x)| Effect {
                        symbol: s.into(),
                        materiality: m,
                        sentiment: x,
                    })
                    .collect(),
            })
            .collect();
        (end, n, Answer { items })
    }

    #[test]
    fn sentiment_decays_from_its_window_and_is_unknown_outside_scored_news() {
        let t0 = Utc.with_ymd_and_hms(2026, 8, 3, 13, 30, 0).unwrap();
        let day = Duration::days(1);
        let mut calls: Vec<_> = (0..10)
            .map(|k| call(t0 + day * k, vec![("0", false, vec![])]))
            .collect();
        calls.push(call(
            t0 + day * 6,
            vec![
                ("0", false, vec![("AMD", 2, 0.5), ("NVDA", 1, 1.0)]),
                ("1", true, vec![("AMD", 3, 1.0)]), // a repeat: told already
                ("2", false, vec![("AMD", 0, -1.0)]), // materiality 0: no effect
                ("9", false, vec![("AMD", 3, 1.0)]), // an id matching no item sent
            ],
        ));
        calls.push(call(
            t0 + day * 8,
            vec![("0", false, vec![("AMD", 1, -1.0)])],
        ));
        calls.push(call(t0 + day * 40, vec![("0", false, vec![])]));
        let news = NewsSentiment::from_calls(calls);
        let half = Duration::days(2); // warm from t0 + 6 days

        assert_eq!(news.at("AMD", t0 + day * 5, half), None); // not yet warm
        let before = news
            .at("AMD", t0 + day * 8 - Duration::minutes(1), half)
            .unwrap();
        assert!((before - 1.0 * 0.5_f64.powf((2.0 * 1440.0 - 1.0) / 2880.0)).abs() < 1e-12);
        let at8 = news.at("AMD", t0 + day * 8, half).unwrap();
        assert!((at8 - (2.0 * 0.5 * 0.5 - 1.0)).abs() < 1e-12);
        assert_eq!(news.at("TSLA", t0 + day * 8, half), Some(0.0)); // scored, no news: zero
        assert_eq!(news.at("AMD", t0 + day * 20, half), None); // the unscored month
    }
}
