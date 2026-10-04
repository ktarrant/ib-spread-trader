//! How thin the near-spot ladder is, and how well `ChainSnapshot::estimate_quote`
//! recovers a strike the capture did not print.
//!
//! Distance from spot is `z = ln(K/F) / (σ√T)`, σ the bar's ATM IV — the units
//! the capture's tapered ladder is planned in.
//!
//! - **density**: per bar, strikes quoted (with a solved IV) within |z| ≤ 0.5
//!   and ≤ 1, against strikes registered for the chain in the same band.
//! - **holdout**: on every bar, drop one quoted strike (both rights), or it and
//!   its two neighbours, and estimate each dropped quote from what is left.
//!   Mid error is in units of the real half-spread; `spread` is estimated over
//!   real. `interp` means the strike lies inside what is left of the OTM smile.
//!
//! `cargo run --release --example estimate_profile`
use chrono::NaiveDate;
use ib_spread_trader::data::{
    ChainSeries, ChainSnapshot, Snapshot, chain_coverage, snapshot::default_snapshot_path,
};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};

const Z_BINS: [f64; 6] = [0.0, 0.25, 0.5, 1.0, 1.5, 2.0];

fn z_bin(z: f64) -> Option<String> {
    let a = z.abs();
    Z_BINS
        .windows(2)
        .find(|w| a >= w[0] && a < w[1])
        .map(|w| format!("{:.2}-{:.2}", w[0], w[1]))
}

fn pct(v: &mut [f64], p: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(f64::total_cmp);
    v[((v.len() - 1) as f64 * p).round() as usize]
}

/// (|mid error| / half-spread, signed mid error / half-spread, est spread / real)
type Errs = (Vec<f64>, Vec<f64>, Vec<f64>);

fn main() -> anyhow::Result<()> {
    let snap = Snapshot::open(default_snapshot_path()?)?;
    let mut holdout: BTreeMap<(usize, &str, String), Errs> = BTreeMap::new();
    let mut density: BTreeMap<&str, Vec<f64>> = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for c in chain_coverage(&snap)?.into_iter().filter(|c| c.is_tradeable()) {
        if !seen.insert((c.symbol.clone(), c.expiry.clone())) {
            continue;
        }
        let s = ChainSeries::load(&snap, &c.symbol, NaiveDate::parse_from_str(&c.expiry, "%Y-%m-%d")?)?;
        let registered: Vec<f64> = snap
            .conn()
            .prepare(
                "SELECT DISTINCT strike FROM contracts
                 WHERE kind = 'option' AND symbol = ?1 AND expiry = ?2",
            )?
            .query_map((&c.symbol, &c.expiry), |r| r.get(0))?
            .collect::<Result<_, _>>()?;
        for i in 0..s.len() {
            let Some(bar) = s.snapshot(i) else { continue };
            let Some(atm) = bar.atm_implied_vol() else { continue };
            let width = atm * bar.year_fraction.sqrt();
            if !(width > 0.0) {
                continue;
            }
            let z = |k: f64| (k / bar.forward).ln() / width;
            let quoted: BTreeSet<u64> = bar
                .quotes
                .iter()
                .filter(|q| q.implied_vol.is_some())
                .map(|q| q.strike.to_bits())
                .collect();
            for (band, name_q, name_r) in [(0.5, "quoted_0.5", "registered_0.5"), (1.0, "quoted_1", "registered_1")] {
                let q = quoted.iter().filter(|&&k| z(f64::from_bits(k)).abs() <= band).count();
                let r = registered.iter().filter(|&&k| z(k).abs() <= band).count();
                density.entry(name_q).or_default().push(q as f64);
                density.entry(name_r).or_default().push(r as f64);
            }

            let strikes: Vec<f64> = quoted.iter().map(|&k| f64::from_bits(k)).collect();
            for (j, &strike) in strikes.iter().enumerate() {
                let zk = z(strike);
                let Some(bin) = z_bin(zk) else { continue };
                for drop in [1usize, 3] {
                    let lo = strikes[j.saturating_sub(drop / 2)];
                    let hi = strikes[(j + drop / 2).min(strikes.len() - 1)];
                    let mut thin: ChainSnapshot = bar.clone();
                    thin.quotes.retain(|q| q.strike < lo || q.strike > hi);
                    let otm: Vec<f64> = thin
                        .quotes
                        .iter()
                        .filter(|q| q.implied_vol.is_some() && (q.right.is_call() == (q.strike >= thin.forward)))
                        .map(|q| q.strike)
                        .collect();
                    let interp = otm.iter().any(|&k| k < strike) && otm.iter().any(|&k| k > strike);
                    for real in bar.quotes.iter().filter(|q| q.strike == strike) {
                        let half = 0.5 * (real.ask - real.bid);
                        let Some(est) = thin.estimate_quote(real.contract_id, strike, real.right) else { continue };
                        if !(half > 0.0) {
                            continue;
                        }
                        let e = holdout
                            .entry((drop, if interp { "interp" } else { "extrap" }, bin.clone()))
                            .or_default();
                        e.0.push((est.mid - real.mid).abs() / half);
                        e.1.push((est.mid - real.mid) / half);
                        e.2.push((est.ask - est.bid) / (real.ask - real.bid));
                    }
                }
            }
        }
    }

    let density: BTreeMap<_, _> = density
        .into_iter()
        .map(|(k, mut v)| {
            let share_le2 = v.iter().filter(|&&x| x <= 2.0).count() as f64 / v.len() as f64;
            (k, json!({"p10": pct(&mut v, 0.1), "median": pct(&mut v, 0.5), "share_le_2": share_le2, "bars": v.len()}))
        })
        .collect();
    let holdout: Vec<_> = holdout
        .into_iter()
        .map(|((drop, kind, bin), (mut abs, mut signed, mut spread))| {
            json!({
                "dropped": drop, "kind": kind, "abs_z": bin, "n": abs.len(),
                "abs_err_half_spreads": {"median": pct(&mut abs, 0.5), "p90": pct(&mut abs, 0.9)},
                "bias_half_spreads": pct(&mut signed, 0.5),
                "spread_ratio_median": pct(&mut spread, 0.5),
            })
        })
        .collect();
    println!("{}", serde_json::to_string_pretty(&json!({"density": density, "holdout": holdout}))?);
    Ok(())
}
