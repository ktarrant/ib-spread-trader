//! What the option tape says about execution, by time of day.
//!
//! For every quote the ranked models could trade (|delta| 0.15-0.65) on every
//! bar of every tradeable chain:
//!
//! - **spread**: median relative spread `(ask - bid) / mid`, slotted by quote
//!   time (a bar's close — `ts` plus its size).
//! - **ATM IV**: each session's ATM IV at that slot over the session's mean.
//! - **limit orders**: a 1-lot limit at `mid ± alpha * half_spread`, placed at
//!   the bar's close. It fills if a later bar of the same session trades the
//!   far side strictly through it (ask low below a buy, bid high above a sell)
//!   within `k` bars; otherwise it chases, crossing on bar `k`. The saving is
//!   against crossing at once, in % of mid. Chasing makes an unfilled order pay
//!   for the drift it waited through, which is where adverse selection shows.
//!
//! `cargo run --release --example execution_profile`
use chrono::{NaiveDate, Timelike};
use chrono_tz::America::New_York;
use ib_spread_trader::data::{
    ChainSeries, Snapshot, chain_coverage, snapshot::default_snapshot_path,
};
use std::collections::{BTreeMap, BTreeSet, HashMap};

const ALPHAS: [f64; 2] = [0.0, 0.5];
const WAITS: [usize; 3] = [1, 2, 4];

/// (fills, saving sum, n)
type Tally = (f64, f64, f64);

fn add(t: &mut Tally, filled: bool, saving: f64) {
    t.0 += f64::from(u8::from(filled));
    t.1 += saving;
    t.2 += 1.0;
}

#[derive(Default)]
struct Slot {
    spreads: Vec<f64>,
    iv_ratio: Vec<f64>,
    buy_mid_2: Tally,
    sell_mid_2: Tally,
}

fn main() -> anyhow::Result<()> {
    let snap = Snapshot::open(default_snapshot_path()?)?;
    let mut limits: BTreeMap<(i64, &str, u32, usize), Tally> = BTreeMap::new();
    let mut slots: BTreeMap<(i64, u32), Slot> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for c in chain_coverage(&snap)?.into_iter().filter(|c| c.is_tradeable()) {
        if !seen.insert((c.symbol.clone(), c.expiry.clone())) {
            continue; // listed once per bar size; ChainSeries takes the finest
        }
        let s = ChainSeries::load(&snap, &c.symbol, NaiveDate::parse_from_str(&c.expiry, "%Y-%m-%d")?)?;
        let size = format!("{}min", s.bar_minutes);
        // (contract, ts) -> (bid high, ask low): the intra-bar extremes the
        // engine never reads, since it prices at the close.
        let mut stmt = snap.conn().prepare(
            "SELECT b.contract_id, b.ts,
                    MAX(CASE WHEN b.data_type = 'bid' THEN b.high END),
                    MAX(CASE WHEN b.data_type = 'ask' THEN b.low END)
             FROM bars b JOIN contracts c ON c.id = b.contract_id
             WHERE c.kind = 'option' AND c.symbol = ?1 AND c.expiry = ?2
               AND b.bar_size = ?3 AND b.data_type IN ('bid', 'ask')
             GROUP BY b.contract_id, b.ts",
        )?;
        let mut range: HashMap<(i64, String), (f64, f64)> = HashMap::new();
        let rows = stmt.query_map((&c.symbol, &c.expiry, &size), |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<f64>>(2)?, r.get::<_, Option<f64>>(3)?))
        })?;
        for row in rows {
            if let (id, ts, Some(bid_high), Some(ask_low)) = row? {
                range.insert((id, ts), (bid_high, ask_low));
            }
        }

        let snaps: Vec<_> = (0..s.len()).filter_map(|i| s.snapshot(i)).collect();
        let quoted = |i: usize| (snaps[i].ts + chrono::Duration::minutes(s.bar_minutes)).with_timezone(&New_York);
        let key = |i: usize| snaps[i].ts.format("%Y-%m-%dT%H:%M:%S+00:00").to_string();
        let mut iv_by_day: BTreeMap<NaiveDate, Vec<(u32, f64)>> = BTreeMap::new();
        for i in 0..snaps.len() {
            let t = quoted(i);
            let slot_key = (s.bar_minutes, t.hour() * 60 + t.minute());
            if let Some(iv) = snaps[i].atm_implied_vol() {
                iv_by_day.entry(t.date_naive()).or_default().push((slot_key.1, iv));
            }
            let slot = slots.entry(slot_key).or_default();
            for q in &snaps[i].quotes {
                if !q.delta.is_some_and(|d| (0.15..=0.65).contains(&d.abs())) || q.mid <= 0.0 {
                    continue;
                }
                slot.spreads.push((q.ask - q.bid) / q.mid);
                let half = 0.5 * (q.ask - q.bid);
                for k in WAITS {
                    if i + k >= snaps.len() || quoted(i + k).date_naive() != t.date_naive() {
                        continue;
                    }
                    // Every bar waited through must quote the contract, or the
                    // order's fate is unknown.
                    let path: Option<Vec<(f64, f64)>> =
                        (1..=k).map(|j| range.get(&(q.contract_id, key(i + j))).copied()).collect();
                    let (Some(path), Some(end)) = (path, snaps[i + k].quote(q.contract_id)) else {
                        continue;
                    };
                    for alpha in ALPHAS {
                        let tag = (alpha * 100.0) as u32;
                        let buy = q.mid + alpha * half;
                        let filled = path.iter().any(|&(_, ask_low)| ask_low < buy);
                        let saving = (q.ask - if filled { buy } else { end.ask }) / q.mid * 100.0;
                        add(limits.entry((s.bar_minutes, "buy", tag, k)).or_default(), filled, saving);
                        if tag == 0 && k == 2 {
                            add(&mut slot.buy_mid_2, filled, saving);
                        }
                        let sell = q.mid - alpha * half;
                        let filled = path.iter().any(|&(bid_high, _)| bid_high > sell);
                        let saving = ((if filled { sell } else { end.bid }) - q.bid) / q.mid * 100.0;
                        add(limits.entry((s.bar_minutes, "sell", tag, k)).or_default(), filled, saving);
                        if tag == 0 && k == 2 {
                            add(&mut slot.sell_mid_2, filled, saving);
                        }
                    }
                }
            }
        }
        for (_, day) in iv_by_day.into_iter().filter(|(_, v)| v.len() >= 10) {
            let mean = day.iter().map(|x| x.1).sum::<f64>() / day.len() as f64;
            for (slot, iv) in day {
                slots.entry((s.bar_minutes, slot)).or_default().iv_ratio.push(iv / mean);
            }
        }
    }

    let pct = |t: &Tally| (t.0 / t.2 * 100.0, t.1 / t.2);
    println!("limit orders, all slots\nbars side  limit          wait n        fill%  saving vs crossing (% of mid)");
    for ((bars, side, alpha, k), t) in &limits {
        let (fill, saving) = pct(t);
        println!("{bars}m  {side:4}  mid+{alpha:>2}% of half  {k}    {:<8} {fill:5.1}  {saving:+.2}", t.2);
    }
    println!("\nby quote time\nbars time  med spread  ATM IV/day  buy@mid,2 bars fill% saving  sell@mid,2 bars fill% saving");
    for ((bars, slot), s) in &mut slots {
        if s.spreads.is_empty() || s.buy_mid_2.2 == 0.0 {
            continue;
        }
        s.spreads.sort_by(f64::total_cmp);
        let iv = s.iv_ratio.iter().sum::<f64>() / s.iv_ratio.len().max(1) as f64;
        let (bf, bs) = pct(&s.buy_mid_2);
        let (sf, ss) = pct(&s.sell_mid_2);
        println!(
            "{bars}m  {:02}:{:02}  {:6.2}%     {iv:.4}      {bf:5.1} {bs:+.2}           {sf:5.1} {ss:+.2}",
            slot / 60, slot % 60, s.spreads[s.spreads.len() / 2] * 100.0
        );
    }
    Ok(())
}
