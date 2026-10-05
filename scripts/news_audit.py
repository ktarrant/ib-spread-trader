#!/usr/bin/env python3
"""Is the LLM headline-score cache sound, and does it say anything about the
stock's next move? Rerun as the backlog fills in.

**Audit.** Every call's answer against what it was sent (unanswered items, ids
that match nothing, values out of range, untracked symbols, an item later than
its window, a future event dated before it), the label mix, and coverage per
New York day: decision windows holding a snapshot headline against windows
with a call. A scored day still misses about one window: one whose headlines
the scorer's noise filter dropped entirely, so it never made a call.

**Event study, on the 5-minute stock tape** (no forecast log needed, so it
reaches the snapshot's last bar). One row per (symbol, window end t) inside a
scored segment (`news_scores.segments`). Entry is the open of the first bar
starting at or after t — a 16:00 or overnight window enters at the next 09:30
open. Returns run to the entry session's close (`rest`), the next session's
(`d1`) and the fifth's (`d5`), minus SPY's over the same span for single
names, in units of the symbol's daily stdev (`d5` over √5). `pre` is the
window itself: the 30 minutes (or the night) before t, already history when
the score arrives; `past` is the move since the prior session's close. "Fresh
news" is a non-repeat effect with materiality >= 1 whose window ends at t. Intervals resample whole days: news clusters, and
~20 days, not ~4 000 rows, is the sample.

Variance is read against the **same entry slot's** quiet rows (no material
news in the prior 24 h, no release within a session): overnight news enters
at the open and holds a whole session, so a raw comparison with rows spread
across the day would read time of day as a news effect.

This audit does **not** apply the coverage mask: before a symbol's
`news_profile.complete_from` its "quiet" rows may hold headlines IB never
returned. `news_study.py` re-runs these findings inside complete coverage.

    python3 scripts/news_audit.py [--cache news-llm.sqlite3] [--db snapshot]
"""

from __future__ import annotations

import argparse
import bisect
import collections
import datetime as dt
import json
import math
import random
import sqlite3
from pathlib import Path
from zoneinfo import ZoneInfo

from news_profile import load
from news_scores import CACHE, _calls, effects_by_symbol, scored, segments, sentiment_index
from option_premium import ETFS, default_db

NY = ZoneInfo("America/New_York")
LEAN = 0.15  # |direction| at or below this is no call


def window_end(t: dt.datetime) -> dt.datetime:
    """The decision window holding t, as ib-option-calc's `score::window_end`:
    30-minute RTH slots, and one window from the close to the next 09:30."""
    local = t.astimezone(NY)
    open_, close = (local.replace(hour=h, minute=m, second=0, microsecond=0) for h, m in ((9, 30), (16, 0)))
    if local.weekday() < 5 and open_ < local <= close:
        slots = -(-int((local - open_).total_seconds()) // 1800)
        return (open_ + dt.timedelta(minutes=30 * slots)).astimezone(dt.timezone.utc)
    day = local.date() if local <= open_ else local.date() + dt.timedelta(days=1)
    while day.weekday() >= 5:
        day += dt.timedelta(days=1)
    return dt.datetime(day.year, day.month, day.day, 9, 30, tzinfo=NY).astimezone(dt.timezone.utc)


def audit(cache: Path, db) -> None:
    calls = list(_calls(cache))
    prob, mat, kind = collections.Counter(), collections.Counter(), collections.Counter()
    windows, sent = set(), 0
    for end, items, raw in calls:
        windows.add(end)
        sent += len(items)
        prob["item after its window"] += sum(dt.datetime.fromisoformat(it["at"]) > end for it in items)
        ids = [a["id"] for a in raw["items"]]
        valid = {str(i) for i in range(len(items))}
        prob["unanswered item"] += len(valid - set(ids))
        prob["answer matching no item"] += len(set(ids) - valid) + len(ids) - len(set(ids))
        for a in raw["items"]:
            kind[a["event_type"], a["is_repeat"]] += 1
            mat[max((e["materiality"] for e in a["effects"]), default=0)] += 1
            for e in a["effects"]:
                prob["value out of range"] += not (0 <= e["materiality"] <= 3 and -1 <= e["direction"] <= 1
                                                   and -1 <= e["sentiment"] <= 1)
                prob["bad vol/horizon"] += e["vol"] not in ("none", "elevated", "high") or \
                    e["horizon"] not in ("intraday", "days", "weeks")
            for f in a["future_events"]:
                try:
                    prob["future event dated before now"] += dt.date.fromisoformat(f["date"]) < end.date()
                except (KeyError, ValueError):
                    prob["future event without a date"] += 1
    universe = {s for _e, items, _r in calls for it in items for s in it["symbols"]}
    n = sum(mat.values())
    print(f"calls {len(calls)}, windows {len(windows)}, items sent {sent}, answered {n}")
    print("checks:", ", ".join(f"{k} {v}" for k, v in prob.items()))
    print("max materiality:", ", ".join(f"{m}: {c} ({c / n:.0%})" for m, c in sorted(mat.items())))
    rep = sum(c for (_k, r), c in kind.items() if r)
    types = collections.Counter()
    for (k, _r), c in kind.items():
        types[k] += c
    print(f"repeats {rep} ({rep / n:.0%}); event types:", ", ".join(f"{k} {c}" for k, c in types.most_common()))

    first = min(windows) - dt.timedelta(days=1)
    by_win = collections.defaultdict(int)
    for r in load(db):
        if r["symbol"] in universe and (w := window_end(r["at"])) >= first:
            by_win[w] += 1
    days = collections.defaultdict(lambda: [0, 0, 0, 0])
    for w, k in by_win.items():
        d = days[w.astimezone(NY).date()]
        d[0] += 1; d[1] += w in windows; d[2] += k; d[3] += k * (w in windows)
    print(f"\n{'NY day':14}{'windows':>8}{'scored':>7}{'headlines':>10}{'in scored':>10}")
    for day, (w, s, h, hs) in sorted(days.items()):
        print(f"{day} {day:%a}{w:8}{s:7}{h:10}{hs:10}  {'#' * round(20 * s / w)}")
    print("segments:", ", ".join(f"{a:%Y-%m-%d %H:%M}..{b:%Y-%m-%d %H:%M}" for a, b in segments(scored(cache))))


def release_days(db) -> dict[str, set[dt.date]]:
    out = collections.defaultdict(set)
    for sym, day in db.execute("SELECT symbol, COALESCE(json_extract(detail_json, '$.reaction_date'), event_date) "
                               "FROM events WHERE kind = 'earnings'"):
        out[sym].add(dt.date.fromisoformat(day))
    return out


def panel(cache: Path, db) -> list[dict]:
    items = scored(cache)
    segs = segments(items)
    tape = collections.defaultdict(list)
    for sym, ts, o, c in db.execute(
        """SELECT c.symbol, ts, open, close FROM bars b JOIN contracts c ON c.id = b.contract_id
           WHERE c.kind = 'stock' AND b.bar_size = '5min' AND b.data_type = 'trades' ORDER BY c.symbol, ts"""
    ):
        tape[sym].append((dt.datetime.fromisoformat(ts), o, c))
    closes, sd = {}, {}
    for sym, bars in tape.items():
        last = {}
        for t, _o, c in bars:
            last[t.date()] = c
        closes[sym] = last
        cs = list(last.values())
        r = [math.log(b / a) for a, b in zip(cs, cs[1:])]
        sd[sym] = math.sqrt(sum(x * x for x in r) / len(r))  # ponytail: full-sample, descriptive only

    def move(sym, t):
        bars = tape[sym]
        i = bisect.bisect_left(bars, (t,))
        if i == 0 or i >= len(bars) or bars[i][0] - t > dt.timedelta(days=4):
            return None
        px, day = bars[i][1], bars[i][0].date()
        j = bisect.bisect_left(bars, (t - dt.timedelta(minutes=30),))
        days = list(closes[sym])
        k = days.index(day)
        out = {h: math.log(closes[sym][days[k + n]] / px) if k + n < len(days) else None
               for h, n in (("rest", 0), ("d1", 1), ("d5", 5))}
        past = math.log(px / closes[sym][days[k - 1]]) if k else None
        return day, dict(out, pre=math.log(px / bars[j - 1][2]) if j else None, past=past)

    grid = []
    for a, b in segs:
        t = a
        while t <= b:
            local = t.astimezone(NY)
            if local.weekday() < 5 and dt.time(9, 30) <= local.time() <= dt.time(16, 0):
                grid.append(t)
            t += dt.timedelta(minutes=30)
    effects = collections.defaultdict(list)
    for it in items:
        for e in it["effects"]:
            if not it["is_repeat"]:
                effects[e["symbol"]].append(dict(e, at=it["available_at"], type=it["event_type"]))
    releases = release_days(db)

    spy = {t: move("SPY", t) for t in grid}
    rows = []
    for sym in tape:
        etf = sym in ETFS
        for t in grid:
            m = move(sym, t)
            if m is None or (not etf and (spy[t] is None or spy[t][0] != m[0])):
                continue
            day, ret = m
            if not etf:
                ret = {h: v - spy[t][1][h] if v is not None and spy[t][1][h] is not None else None for h, v in ret.items()}
            now = [e for e in effects[sym] if e["at"] == t and e["materiality"] >= 1]
            w = sum(e["materiality"] for e in now)
            local = t.astimezone(NY).time()
            rows.append(dict(
                symbol=sym, t=t, day=day, etf=etf, slot="open" if local in (dt.time(9, 30), dt.time(16, 0)) else f"{local:%H:%M}",
                earnings=any(abs((day - d).days) <= 1 for d in releases[sym]),
                mat=max((e["materiality"] for e in now), default=0),
                vol=max((("none", "elevated", "high").index(e["vol"]) for e in now), default=-1),
                types={e["type"] for e in now},
                dirn=sum(e["materiality"] * e["direction"] for e in now) / w if w else None,
                sent=sum(e["materiality"] * e["sentiment"] for e in now) / w if w else None,
                quiet=any(a <= t - dt.timedelta(hours=24) and t <= b for a, b in segs)
                and not any(t - dt.timedelta(hours=24) < e["at"] <= t and e["materiality"] >= 1 for e in effects[sym]),
                z={h: v / sd[sym] / (math.sqrt(5) if h == "d5" else 1) if v is not None else None for h, v in ret.items()},
            ))
    # Squared move against the same slot's quiet, release-free rows.
    for h in ("pre", "rest", "d1", "d5"):
        base = collections.defaultdict(list)
        for r in rows:
            if r["quiet"] and not r["earnings"] and r["z"][h] is not None:
                base[r["etf"], r["slot"]].append(r["z"][h] ** 2)
        mean = {k: sum(v) / len(v) for k, v in base.items()}
        for r in rows:
            k = (r["etf"], r["slot"])
            r[h + "_var"] = r["z"][h] ** 2 / mean[k] if r["z"][h] is not None and mean.get(k) else None
    return rows


def interval(rows, f, n=2000):
    """Mean of f, with a 95 % interval resampling whole days; None under 3 days."""
    by = collections.defaultdict(list)
    for r in rows:
        if (v := f(r)) is not None:
            by[r["day"]].append(v)
    if len(by) < 3:
        return None
    days, rnd, means = list(by), random.Random(1), []
    for _ in range(n):
        s = [v for d in rnd.choices(days, k=len(days)) for v in by[d]]
        means.append(sum(s) / len(s))
    means.sort()
    vals = [v for d in days for v in by[d]]
    return sum(vals) / len(vals), means[n // 40], means[n - n // 40 - 1], len(vals), len(days)


def table(title, groups, cols, sign=True):
    print(f"\n{title}\n  {'bucket':22}" + "".join(f"{c:>27}" for c in cols))
    for name, rs in groups:
        cells = []
        for f in cols.values():
            iv = interval(rs, f)
            fmt = "+.2f" if sign else ".2f"
            cells.append(f"{iv[0]:{fmt}} [{iv[1]:{fmt}}, {iv[2]:{fmt}}] {iv[3]}" if iv else "—")
        print(f"  {name:22}" + "".join(f"{c:>27}" for c in cells))


def events(rows) -> None:
    single = [r for r in rows if not r["etf"]]
    fresh = [r for r in single if r["mat"] >= 1 and not r["earnings"]]
    print(f"\n=== event study: {len(single)} single-name rows over {len({r['day'] for r in single})} days, "
          f"{len(fresh)} with fresh material news ex-releases ===")
    sgn = lambda x: (x > LEAN) - (x < -LEAN)
    for key, label in (("dirn", "LLM direction"), ("sent", "sentiment")):
        called = [r for r in fresh if sgn(r[key])]
        signed = lambda h: lambda r: sgn(r[key]) * r["z"][h] if r["z"][h] is not None else None
        table(f"{label}: return in its direction, sd units (ex-releases)", [
            ("all", called), *((f"materiality {m}", [r for r in called if r["mat"] == m]) for m in (1, 2, 3)),
            ("overnight", [r for r in called if r["slot"] == "open"]),
            ("intraday", [r for r in called if r["slot"] != "open"]),
        ], {"pre": signed("pre"), "rest": signed("rest"), "d1": signed("d1"), "d5": signed("d5"),
            "d1 hit rate": lambda r: float(sgn(r[key]) * r["z"]["d1"] > 0) if r["z"]["d1"] is not None else None})
    var = lambda h: lambda r: r[h + "_var"]
    table("squared move / same-slot quiet mean (1.00 = a quiet bar at that time)", [
        ("quiet 24h (control)", [r for r in single if r["quiet"] and not r["earnings"]]),
        ("fresh news", fresh), *((f"materiality {m}", [r for r in fresh if r["mat"] == m]) for m in (1, 2, 3)),
        ("vol none", [r for r in fresh if r["vol"] == 0]),
        ("vol elevated/high", [r for r in fresh if r["vol"] >= 1]),
        ("release day, news", [r for r in single if r["mat"] >= 1 and r["earnings"]]),
    ], {h: var(h) for h in ("pre", "rest", "d1", "d5")}, sign=False)
    table("by event type (ex-releases)", [
        (k, [r for r in fresh if k in r["types"]]) for k in sorted({k for r in fresh for k in r["types"]})
    ], {"d1 in LLM direction": lambda r: sgn(r["dirn"]) * r["z"]["d1"] if r["dirn"] is not None
        and sgn(r["dirn"]) and r["z"]["d1"] is not None else None,
        "d1 variance": var("d1")})


def sentiment(rows, items) -> None:
    """The decayed sentiment indicator (`news_scores.sentiment_index`): where
    it stands now, and whether its tone (sentiment / intensity, in terciles
    across rows) or its intensity says anything about direction, continuation
    of the move since the prior close, or variance. A row counts only once the
    scored history covers three half-lives before it."""
    eff, segs = effects_by_symbol(items), segments(items)
    end = segs[-1][1]
    now = [(s / i, i, sym) for sym, es in eff.items()
           for s, i in [sentiment_index(es, end, dt.timedelta(hours=48))] if i > 0.5]
    print(f"\n=== sentiment, 48h half-life, as of {end:%Y-%m-%d %H:%M} UTC: tone (intensity) ===")
    print("  " + "  ".join(f"{sym} {tone:+.2f} ({i:.0f})" for tone, i, sym in sorted(now, reverse=True)))
    sg = lambda x: (x > 0) - (x < 0)
    for hours in (24, 48):
        half = dt.timedelta(hours=hours)
        warm = [dict(r) for r in rows if not r["etf"] and not r["earnings"]
                and any(a <= r["t"] - 3 * half and r["t"] <= b for a, b in segs)]
        for r in warm:
            s, i = sentiment_index(eff.get(r["symbol"], []), r["t"], half)
            r["tone"], r["intensity"] = s / i if i else 0.0, i
        cut = lambda k: sorted(r[k] for r in warm)
        tones, ints = cut("tone"), cut("intensity")
        third = lambda xs, r_: 0 if r_ < xs[len(xs) // 3] else 2 if r_ > xs[2 * len(xs) // 3] else 1
        for r in warm:
            r["tband"], r["iband"] = third(tones, r["tone"]), third(ints, r["intensity"])
        print(f"\n--- half-life {hours}h: {len(warm)} rows over {len({r['day'] for r in warm})} days, "
              f"tone terciles at {tones[len(tones) // 3]:+.2f} / {tones[2 * len(tones) // 3]:+.2f} ---")
        tercile = ("low", "mid", "high")
        z = lambda h: lambda r: r["z"][h]
        table("direction: return by tone tercile", [(f"tone {n}", [r for r in warm if r["tband"] == k])
              for k, n in enumerate(tercile)], {h: z(h) for h in ("past", "rest", "d1", "d5")})
        moved = [r for r in warm if r["z"]["past"] is not None and abs(r["z"]["past"]) > 0.5]
        agrees = lambda r: r["tband"] != 1 and (r["tband"] == 2) == (r["z"]["past"] > 0)
        cont = lambda h: lambda r: sg(r["z"]["past"]) * r["z"][h] if r["z"][h] is not None else None
        table("momentum: after a >0.5 sd move since the prior close, return in its direction", [
            ("all moves", moved), ("tone agrees", [r for r in moved if agrees(r)]),
            ("tone opposes", [r for r in moved if r["tband"] != 1 and not agrees(r)]),
            ("tone mid", [r for r in moved if r["tband"] == 1])], {h: cont(h) for h in ("rest", "d1", "d5")})
        table("variance / same-slot quiet mean", [
            *((f"intensity {n}", [r for r in warm if r["iband"] == k]) for k, n in enumerate(tercile)),
            ("tone low", [r for r in warm if r["tband"] == 0]), ("tone high", [r for r in warm if r["tband"] == 2]),
        ], {h: (lambda h: lambda r: r[h + "_var"])(h) for h in ("rest", "d1", "d5")}, sign=False)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--cache", default=CACHE, type=Path)
    ap.add_argument("--db", default=None)
    args = ap.parse_args()
    db = sqlite3.connect(f"file:{args.db or default_db()}?mode=ro", uri=True)
    audit(args.cache, db)
    rows, items = panel(args.cache, db), scored(args.cache)
    events(rows)
    sentiment(rows, items)


if __name__ == "__main__":
    main()
