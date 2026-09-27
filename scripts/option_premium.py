#!/usr/bin/env python3
"""Where the option premium is, measured straight off the quotes — no model.

Two views, both entering at 15:00 UTC each day (midday: the first and last bars
of a session quote about twice the spread) and exiting at the same time a fixed
number of days later:

    map         Every option, bucketed by right and distance out of the money:
                the seller's return on premium at mid, delta-hedged with the
                entry delta (Black-76 on the quote), so the market's direction
                is taken out and what remains is the premium itself.
    structures  Defined-risk ETF structures priced leg by leg, crossing every
                leg both ways plus commission: return on max loss (the collateral
                a no-leverage account ties up), by hold length and entry month,
                and how much fill quality would change it.

Every chain in the snapshot is read at its finest option bar size; periods are
calendar months of entry. Chains that overlap in time each contribute their own
entries (different contracts, same days), so a month two chains share counts
those days twice. Re-run it on each new capture — each one adds a period,
and a pattern that holds across more of them is the only kind worth trading.

    python3 scripts/option_premium.py map
    python3 scripts/option_premium.py structures --hold 7 21 28
    python3 scripts/option_premium.py --db snapshot.sqlite3 structures
"""

from __future__ import annotations

import argparse
import collections
import datetime as dt
import math
import random
import re
import sqlite3
from pathlib import Path

ETFS = ("SPY", "QQQ", "IWM")
COMMISSION = 0.65  # per leg per contract, as in the backtester
ENTRY_TIME = "15:00"
TOLERANCE = 0.01  # a leg's strike must sit within 1 % of spot of its target
N = lambda x: 0.5 * (1 + math.erf(x / math.sqrt(2)))

# (right, +1 long / -1 short, distance out of the money as a fraction of spot)
STRUCTURES = {
    "put spread 3/6": [("P", -1, 0.03), ("P", 1, 0.06)],
    "put spread 4.5/6.5": [("P", -1, 0.045), ("P", 1, 0.065)],
    "put spread 6/9": [("P", -1, 0.06), ("P", 1, 0.09)],
    "skewed condor p4.5/6.5 c1/3": [("P", -1, 0.045), ("P", 1, 0.065), ("C", -1, 0.01), ("C", 1, 0.03)],
    "skewed condor p4.5/6.5 c2/4": [("P", -1, 0.045), ("P", 1, 0.065), ("C", -1, 0.02), ("C", 1, 0.04)],
    "symmetric condor 4.5/6.5": [("P", -1, 0.045), ("P", 1, 0.065), ("C", -1, 0.045), ("C", 1, 0.065)],
}


def default_db() -> Path:
    """The snapshot whose filename ends in the latest date — the CLI's rule."""
    files = list((Path.home() / "Documents/Options Database").glob("*.sqlite3"))
    dated = lambda p: re.search(r"(\d{4}-\d{2}-\d{2})$", p.stem)
    return max(files, key=lambda p: (dated(p).group(1) if dated(p) else "", p.name))


def black76(F, K, T, v, call):
    d1 = (math.log(F / K) + 0.5 * v * v * T) / (v * math.sqrt(T))
    d2 = d1 - v * math.sqrt(T)
    return F * N(d1) - K * N(d2) if call else K * N(-d2) - F * N(-d1)


def delta(F, K, T, price, call):
    """Black-76 delta at the vol that reprices `price`; None when nothing does."""
    lo, hi = 1e-3, 5.0
    if T <= 0 or not (black76(F, K, T, lo, call) < price < black76(F, K, T, hi, call)):
        return None
    for _ in range(50):
        mid = (lo + hi) / 2
        lo, hi = (mid, hi) if black76(F, K, T, mid, call) < price else (lo, mid)
    v = (lo + hi) / 2
    d1 = (math.log(F / K) + 0.5 * v * v * T) / (v * math.sqrt(T))
    return N(d1) if call else N(d1) - 1


class Snapshot:
    def __init__(self, path: Path, symbols: tuple[str, ...] | None):
        self.con = sqlite3.connect(f"file:{path}?mode=ro", uri=True)
        where = f"AND c.symbol IN ({','.join('?' * len(symbols))})" if symbols else ""
        self.args = tuple(symbols or ())
        self.where = where
        self.spot = {
            (sym, ts): close
            for sym, ts, close in self.con.execute(
                f"""SELECT c.symbol, b.ts, b.close FROM bars b JOIN contracts c ON c.id = b.contract_id
                    WHERE c.kind = 'stock' AND b.bar_size = '5min' AND b.data_type = 'trades' {where}""",
                self.args,
            )
        }
        self.releases = collections.defaultdict(list)
        for sym, react in self.con.execute(
            """SELECT symbol, COALESCE(json_extract(detail_json, '$.reaction_date'), event_date)
               FROM events WHERE kind = 'earnings'"""
        ):
            self.releases[sym].append(dt.date.fromisoformat(react))

    def chains(self):
        """Every expiry with option bid bars, at its finest bar size (minutes)."""
        found = collections.defaultdict(list)
        for expiry, size in self.con.execute(
            f"""SELECT DISTINCT c.expiry, b.bar_size FROM bars b JOIN contracts c ON c.id = b.contract_id
                WHERE c.kind = 'option' AND b.data_type = 'bid' AND b.bar_size IN ('15min', '30min') {self.where}""",
            self.args,
        ):
            found[expiry].append(int(size[:-3]))
        return sorted((expiry, min(sizes)) for expiry, sizes in found.items())

    def ladders(self, expiry: str, minutes: int):
        """(symbol, ts) -> {(right, strike): (bid, ask)} at the daily entry time."""
        out = collections.defaultdict(dict)
        for sym, strike, right, ts, bid, ask in self.con.execute(
            f"""SELECT c.symbol, c.strike, c.right, b.ts,
                       MAX(CASE WHEN b.data_type = 'bid' THEN b.close END),
                       MAX(CASE WHEN b.data_type = 'ask' THEN b.close END)
                FROM bars b JOIN contracts c ON c.id = b.contract_id
                WHERE c.kind = 'option' AND c.expiry = ? AND b.bar_size = ?
                  AND b.data_type IN ('bid', 'ask') AND substr(b.ts, 12, 5) = ? {self.where}
                GROUP BY c.id, b.ts""",
            (expiry, f"{minutes}min", ENTRY_TIME, *self.args),
        ):
            if bid is not None and ask is not None and 0 < bid < ask:
                out[(sym, ts)][(right, strike)] = (bid, ask)
        return out

    def spot_at(self, sym, bar_start: dt.datetime, minutes: int):
        """The 5-minute close that coincides with the option bar's close."""
        return self.spot.get((sym, (bar_start + dt.timedelta(minutes=minutes - 5)).isoformat()))


def holds(snap: Snapshot, days: int):
    """Every (chain, symbol, entry) with quotes at entry and `days` later."""
    for expiry, minutes in snap.chains():
        ladders = snap.ladders(expiry, minutes)
        expires = dt.datetime.fromisoformat(expiry + "T20:00:00+00:00")
        for (sym, ts), ladder in ladders.items():
            t0 = dt.datetime.fromisoformat(ts)
            later = ladders.get((sym, (t0 + dt.timedelta(days=days)).isoformat()))
            s0 = snap.spot_at(sym, t0, minutes)
            s1 = snap.spot_at(sym, t0 + dt.timedelta(days=days), minutes)
            if later and s0 and s1:
                T = (expires - (t0 + dt.timedelta(minutes=minutes))).total_seconds() / 31557600
                yield expiry, sym, t0, ladder, later, s0, s1, T


def weekly_interval(rows, num, den, rnd, reps=1000):
    """Pooled ratio of sums and its 95 % weekly-block bootstrap interval."""
    by = collections.defaultdict(lambda: [0.0, 0.0])
    for r in rows:
        week = r["day"] - dt.timedelta(days=r["day"].weekday())
        by[week][0] += num(r)
        by[week][1] += den(r)
    blocks = list(by.values())
    point = sum(b[0] for b in blocks) / sum(b[1] for b in blocks)
    boot = sorted(
        sum(x[0] for x in s) / sum(x[1] for x in s)
        for s in ([rnd.choice(blocks) for _ in blocks] for _ in range(reps))
    )
    return point, boot[int(0.025 * reps)], boot[int(0.975 * reps) - 1], len(blocks)


def premium_map(snap: Snapshot, days: int) -> None:
    rows = []
    for expiry, sym, t0, ladder, later, s0, s1, T in holds(snap, days):
        for (right, strike), (bid, ask) in ladder.items():
            if (right, strike) not in later:
                continue
            mid0 = (bid + ask) / 2
            d = delta(s0, strike, T, mid0, right == "C")
            if mid0 < 0.10 or d is None:
                continue
            mid1 = sum(later[(right, strike)]) / 2
            otm = (s0 - strike) / s0 if right == "P" else (strike - s0) / s0
            earn = any(t0.date() < e <= t0.date() + dt.timedelta(days=days) for e in snap.releases[sym])
            group = "ETF" if sym in ETFS else ("stock, earnings in hold" if earn else "stock, no earnings")
            rows.append(dict(expiry=expiry, day=t0.date(), right=right, otm=otm, group=group, mid0=mid0,
                             seller=-(mid1 - mid0 - d * (s1 - s0))))
    buckets = [("ATM", -0.01, 0.01), ("1-3%", 0.01, 0.03), ("3-6%", 0.03, 0.06), ("6-10%", 0.06, 0.10), (">10%", 0.10, 9)]
    rnd = random.Random(1)
    chains = sorted({r["expiry"] for r in rows})
    print(f"Delta-hedged seller return on premium, {days}-day hold, at mid [95 % weekly interval]\n")
    for group in ("ETF", "stock, no earnings", "stock, earnings in hold"):
        print(group)
        print(f"  {'':12}" + "".join(f"{'chain ' + c:>34}" for c in chains))
        for right in ("P", "C"):
            for name, lo, hi in buckets:
                line = f"  {('put' if right == 'P' else 'call') + ' ' + name:12}"
                for c in chains:
                    rs = [r for r in rows if r["group"] == group and r["expiry"] == c and r["right"] == right and lo <= r["otm"] < hi]
                    if len(rs) < 30:
                        line += f"{'—':>34}"
                        continue
                    m, a, b, _ = weekly_interval(rs, lambda r: r["seller"], lambda r: r["mid0"], rnd)
                    line += f"{len(rs):>7} {m * 100:+6.1f}% [{a * 100:+6.1f},{b * 100:+6.1f}]"
                print(line)
        print()


def structures(snap: Snapshot, hold_days: list[int]) -> None:
    rnd = random.Random(2)
    for days in hold_days:
        rows = []
        for expiry, sym, t0, ladder, later, s0, s1, T in holds(snap, days):
            if sym not in ETFS:
                continue
            for name, spec in STRUCTURES.items():
                legs = []
                for right, qty, dist in spec:
                    target = s0 * (1 - dist) if right == "P" else s0 * (1 + dist)
                    strikes = [k for (r, k) in ladder if r == right and (r, k) in later]
                    nearest = min(strikes, key=lambda k: abs(k - target)) if strikes else None
                    # Refused, not moved: a capture band that ends at -5 % would
                    # otherwise turn a 6.5 % wing into the last strike quoted.
                    if nearest is not None and abs(nearest - target) <= TOLERANCE * s0:
                        legs.append((right, qty, nearest))
                if len(legs) != len(spec) or len({(r, k) for r, _, k in legs}) != len(legs):
                    continue
                entry = sum(q * (ladder[(r, k)][1] if q > 0 else ladder[(r, k)][0]) for r, q, k in legs)
                exit_ = sum(q * (later[(r, k)][0] if q > 0 else later[(r, k)][1]) for r, q, k in legs)
                mid = lambda book: sum(q * sum(book[(r, k)]) / 2 for r, q, k in legs)
                commission = 2 * COMMISSION * len(legs)
                payoff = lambda S: sum(q * max((S - k) if r == "C" else (k - S), 0.0) for r, q, k in legs)
                points = [0.0] + sorted(k for _, _, k in legs) + [2 * max(k for _, _, k in legs)]
                if payoff(points[-1] + 1) < payoff(points[-1]) - 1e-9:
                    continue  # a runaway loss is not defined risk
                max_loss = (entry - min(payoff(p) for p in points)) * 100 + commission
                if max_loss <= 0:
                    continue
                rows.append(dict(structure=name, sym=sym, day=t0.date(), month=t0.strftime("%Y-%m"),
                                 pnl=(exit_ - entry) * 100 - commission,
                                 spread_paid=(mid(later) - mid(ladder)) * 100 - (exit_ - entry) * 100,
                                 max_loss=max_loss))
        months = sorted({r["month"] for r in rows})
        print(f"\n{days}-day hold: return on max loss per trade, unhedged, crossing every leg + commission")
        print(f"{'structure':30}{'fill':>10}{'all [95% weekly]':>30}" + "".join(f"{m:>10}" for m in months) + "  by ETF")
        for name in STRUCTURES:
            rs = [r for r in rows if r["structure"] == name]
            if len(rs) < 20:
                continue
            for share, label in ((0.0, "crossed"), (0.5, "half back")):
                pnl = lambda r: r["pnl"] + share * r["spread_paid"]
                m, a, b, weeks = weekly_interval(rs, pnl, lambda r: r["max_loss"], rnd)
                line = f"{name if share == 0 else '':30}{label:>10}{m * 100:+9.1f}% [{a * 100:+6.1f},{b * 100:+6.1f}] {weeks:2}w"
                for month in months:
                    ms = [r for r in rs if r["month"] == month]
                    line += f"{sum(map(pnl, ms)) / sum(r['max_loss'] for r in ms) * 100:>+9.1f}%" if len(ms) >= 5 else f"{'—':>10}"
                if share == 0:
                    by = collections.defaultdict(list)
                    for r in rs:
                        by[r["sym"]].append(r)
                    line += "  " + " ".join(
                        f"{s} {sum(r['pnl'] for r in v) / sum(r['max_loss'] for r in v) * 100:+.0f}%" for s, v in sorted(by.items())
                    )
                print(line)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--db", type=Path, help="snapshot (default: the newest by the date in its name)")
    sub = parser.add_subparsers(dest="command", required=True)
    m = sub.add_parser("map", help="delta-hedged option returns by right and moneyness")
    m.add_argument("--hold", type=int, default=7)
    s = sub.add_parser("structures", help="defined-risk ETF structures, priced leg by leg")
    s.add_argument("--hold", type=int, nargs="+", default=[7, 21, 28])
    args = parser.parse_args()
    db = args.db or default_db()
    print(f"snapshot {db.name}")
    if args.command == "map":
        premium_map(Snapshot(db, None), args.hold)
    else:
        structures(Snapshot(db, ETFS), args.hold)


if __name__ == "__main__":
    main()
