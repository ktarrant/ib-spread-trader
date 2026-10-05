#!/usr/bin/env python3
"""Do ARK's daily ETF trades in our cohort say anything about the next move?

One row per (single-name stock, trading day T) on the snapshot's daily tape.
ARK trades during T and publishes after the close, so the earliest fill is
T+1's open: `gap` is close T -> open T+1 (the reaction to the email), and the
forward returns run from that open to the close of T+1, T+5 and T+20. `past`
is close T-5 -> close T, `day` the session ARK traded in. All are minus SPY
(beta 1), in units of the symbol's daily excess stdev (scaled by sqrt of the
horizon). A row's ARK direction is the sign of the net fund weight traded
across ARKK/ARKW/ARKQ/..., and `in dir` multiplies the move by it, so a
positive forward number is skill. `demeaned` subtracts the symbol's own mean
daily excess move over the window, so a name ARK only ever bought in a rising
month does not read as foresight. Intervals resample whole weeks: ARK trades
in runs (AMD was sold on 32 days), so a week, not a row, is the sample.

    python3 scripts/ark_study.py [--db snapshot]
"""

from __future__ import annotations

import argparse
import collections
import datetime as dt
import json
import math
import sqlite3

from news_audit import interval
from option_premium import ETFS, default_db

H = {"gap": 0, "d1": 1, "d5": 5, "d20": 20}


def rows(db) -> list[dict]:
    tape = collections.defaultdict(list)
    for sym, ts, o, c in db.execute(
        """SELECT c.symbol, ts, open, close FROM bars b JOIN contracts c ON c.id = b.contract_id
           WHERE c.kind = 'stock' AND b.bar_size = '1d' AND b.data_type = 'trades' ORDER BY c.symbol, ts"""
    ):
        tape[sym].append((dt.date.fromisoformat(ts[:10]), o, c))
    flow = collections.defaultdict(float)
    for sym, day, kind, detail in db.execute("SELECT symbol, event_date, kind, detail_json FROM events WHERE kind IN ('ark_buy', 'ark_sell')"):
        flow[sym, dt.date.fromisoformat(day)] += (1 if kind == "ark_buy" else -1) * json.loads(detail)["etf_percent"]
    spy = {d: (o, c) for d, o, c in tape["SPY"]}

    out = []
    for sym, bars in tape.items():
        if sym in ETFS or sym not in {s for s, _ in flow}:
            continue
        bars = [b for b in bars if b[0] in spy]
        ex = lambda a, b: math.log(b / a)
        xs = lambda i, j, pa, pb: ex(bars[i][pa], bars[j][pb]) - ex(spy[bars[i][0]][pa - 1], spy[bars[j][0]][pb - 1])
        daily = [xs(i - 1, i, 2, 2) for i in range(1, len(bars))]
        mu = sum(daily) / len(daily)
        sd = math.sqrt(sum((x - mu) ** 2 for x in daily) / len(daily))  # ponytail: full-sample, descriptive only
        for i in range(5, len(bars) - 1):
            day = bars[i][0]
            f = flow.get((sym, day), 0.0)
            r = dict(past=(xs(i - 5, i, 2, 2), 5), day=(xs(i - 1, i, 2, 2), 1), gap=(xs(i, i + 1, 2, 1), 1))
            for h, n in H.items():
                if n and i + n < len(bars):
                    r[h] = (xs(i + 1, i + n, 1, 2), n)
            prior = [flow.get((sym, bars[k][0]), 0.0) for k in range(i - 5, i)]
            out.append(dict(
                symbol=sym, t=day, day=day - dt.timedelta(days=day.weekday()),  # week block for `interval`
                dir=(f > 0) - (f < 0), weight=abs(f),
                lead=f != 0 and not any(p * f > 0 for p in prior),
                z={h: v / (sd * math.sqrt(n)) for h, (v, n) in r.items()},
                dz={h: (v - mu * n) / (sd * math.sqrt(n)) for h, (v, n) in r.items()},
            ))
    return out


def show(title, groups, key="z"):
    cols = ("past", "day", "gap", "d1", "d5", "d20")
    print(f"\n{title}\n  {'':24}" + "".join(f"{c:>22}" for c in cols))
    for name, rs, sign in groups:
        cells = []
        for h in cols:
            iv = interval(rs, lambda r: sign(r) * r[key][h] if h in r[key] else None)
            cells.append(f"{iv[0]:+.2f} [{iv[1]:+.2f},{iv[2]:+.2f}]" if iv else "—")
        n = len(rs)
        print(f"  {name:16}{n:>5}{len({r['day'] for r in rs}):>3}w" + "".join(f"{c:>22}" for c in cells))


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--db", default=default_db())
    db = sqlite3.connect(f"file:{ap.parse_args().db}?mode=ro", uri=True)
    rs = rows(db)
    ark = [r for r in rs if r["dir"]]
    print(f"{len(rs)} symbol-days, {len(ark)} with ARK flow, {min(r['t'] for r in rs)} -> {max(r['t'] for r in rs)}")
    by = collections.Counter((r["symbol"], r["dir"]) for r in ark)
    print("  " + "  ".join(f"{s} {by[s, 1]}b/{by[s, -1]}s" for s in sorted({s for s, _ in by})))
    d = lambda r: r["dir"]
    for key, what in (("z", "raw"), ("dz", "demeaned by the symbol's own drift")):
        show(f"In ARK's direction, sd units ({what}); weekly-block 95 % intervals", [
            ("all ARK days", ark, d),
            ("buys", [r for r in ark if r["dir"] > 0], d),
            ("sells", [r for r in ark if r["dir"] < 0], d),
            ("run leading edge", [r for r in ark if r["lead"]], d),
            ("weight >= 0.10 %", [r for r in ark if r["weight"] >= 0.10], d),
        ], key)
    show("Unconditional, no ARK flow that day (raw, up = +)", [("no ARK", [r for r in rs if not r["dir"]], lambda r: 1)])


if __name__ == "__main__":
    main()
