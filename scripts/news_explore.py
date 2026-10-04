#!/usr/bin/env python3
"""Does the news flow say anything about the move to come? Baselines first.

Joins point-in-time news features to a run's `forecasts.csv` and buckets three
targets by them, each with a 95 % weekly-block bootstrap interval:

    var7   realized ÷ implied variance over the forecast horizon (7 days on the
           vol model): ln(realized/spot)² against atm_iv²·horizon, **within
           symbol** — divided by the symbol's own pooled ratio, so 1.00 is
           "a normal move for this name" and ETFs moving less than implied
           cannot pose as a news effect.
    var1   the same over ~one session, spot to the next session's same bar.
    skill  CRPS skill of the run's model against its null.

A forecast at bar `ts` (the bar START) is decided at its close, so it sees only
headlines with `event_time <= ts + bar`. Rows whose trailing window reaches
before the symbol's complete news coverage (news_profile.complete_from) are
dropped as unknown rather than counted as quiet. ETFs are left out: they carry
almost no company headlines.

With a news_score.py cache present, LLM tables follow: vol by materiality,
vol flag and cross-symbol read-through, and **direction** — headline
sentiment, the model's direction, and its per-symbol tone, each bucketed
negative/neutral/positive against the share of rows that went up and the mean
return in implied-move units (z) at 1 and 7 days. Compare buckets with each
other, not with 50 %: the sample has its own drift. A score is read only once
its window has closed (`available_at <= decision`).

    python3 scripts/news_explore.py [--forecasts out/vol_timing_all/forecasts.csv]
"""

from __future__ import annotations

import argparse
import bisect
import collections
import csv
import datetime as dt
import math
import random
import sqlite3
from pathlib import Path

from news_profile import complete_from, load, parse, stories
from news_score import CACHE, scored, symbol_sentiment
from option_premium import ETFS, default_db, weekly_interval

ROOT = Path(__file__).resolve().parent.parent
YEAR = 365 * 86400
HALF_LIFE_H = 24
LEAN = 0.15  # |signal| below this is neutral


def llm_index(cache: Path):
    """Per symbol, time-sorted effects on it (cross-symbol included) and the
    model's tone readings; plus the first and last scored window. Rows outside
    that range are unknown. ponytail: assumes the scored range is contiguous
    (a finished news_score.py run); a failed call inside it reads as quiet."""
    effects, tone = collections.defaultdict(list), collections.defaultdict(list)
    items = scored(cache)
    for it in items:
        for e in it["effects"]:
            cross = e["symbol"] not in it["symbols"]
            effects[e["symbol"]].append(dict(e, at=it["available_at"], cross=cross, repeat=it["is_repeat"]))
    for t in symbol_sentiment(cache):
        tone[t["symbol"]].append(t)
    for d in (effects, tone):
        for v in d.values():
            v.sort(key=lambda e: e["available_at"] if "available_at" in e else e["at"])
    span = (min(it["available_at"] for it in items), max(it["available_at"] for it in items)) if items else None
    return effects, tone, span


def llm_features(sym, t, effects, tone) -> dict:
    window = [e for e in effects.get(sym, []) if t - dt.timedelta(hours=72) < e["at"] <= t]
    assert all(e["at"] <= t for e in window)
    live = [e for e in window if e["materiality"] >= 1]

    def lean(key):
        w = [e["materiality"] * 0.5 ** ((t - e["at"]).total_seconds() / 3600 / HALF_LIFE_H) for e in live]
        return sum(wi * e[key] for wi, e in zip(w, live)) / sum(w) if live else None

    readings = [x for x in tone.get(sym, []) if t - dt.timedelta(hours=72) < x["available_at"] <= t]
    last = [x["sentiment"] for x in readings if x["available_at"] == readings[-1]["available_at"]] if readings else []
    return dict(
        mat=max((e["materiality"] for e in window if not e["repeat"]), default=0),
        volflag=any(e["vol"] != "none" and e["materiality"] >= 1 and not e["repeat"] for e in window),
        cross=any(e["cross"] and e["materiality"] >= 2 for e in window),
        sent=lean("sentiment"), dirn=lean("direction"),
        tone=sum(last) / len(last) if last else None,
    )


def releases(db) -> dict[str, list[dt.date]]:
    """EDGAR release reaction sessions per symbol."""
    out = collections.defaultdict(list)
    for sym, day in db.execute(
        "SELECT symbol, COALESCE(json_extract(detail_json, '$.reaction_date'), event_date) "
        "FROM events WHERE kind = 'earnings'"
    ):
        out[sym].append(dt.date.fromisoformat(day))
    return out


def forecast_rows(path: Path) -> list[dict]:
    rows = [dict(r, at=parse(r["ts"])) for r in csv.DictReader(path.open())]
    by = collections.defaultdict(list)
    for r in rows:
        by[r["symbol"]].append(r)
    for rs in by.values():
        rs.sort(key=lambda r: r["at"])
        gaps = [(b["at"] - a["at"]).total_seconds() for a, b in zip(rs, rs[1:])]
        bar = dt.timedelta(seconds=min(g for g in gaps if g > 0))
        ts = [r["at"] for r in rs]
        for r in rs:
            r["decided"] = r["at"] + bar
            # The next session's same bar (Friday -> Monday), within 4 days.
            j = bisect.bisect_left(ts, r["at"] + dt.timedelta(hours=20))
            nxt = rs[j] if j < len(rs) and rs[j]["at"] - r["at"] < dt.timedelta(days=4) else None
            r["next"] = nxt
    return rows


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--db", default=None)
    ap.add_argument("--forecasts", default=ROOT / "out/vol_timing_all/forecasts.csv", type=Path)
    ap.add_argument("--cache", default=CACHE, type=Path)
    args = ap.parse_args()
    db = sqlite3.connect(f"file:{args.db or default_db()}?mode=ro", uri=True)

    news = collections.defaultdict(list)
    for r in load(db):
        news[r["symbol"]].append(r)
    starts, story_at, ratings = {}, {}, {}
    for sym, rs in news.items():
        starts[sym] = complete_from(rs) or min(r["at"] for r in rs)
        story_at[sym] = [s[0]["at"] for s in stories(rs)]
        ratings[sym] = sorted(r["at"] for r in rs if r["provider"] == "BRFUPDN")
    rel = releases(db)
    effects, tone, span = llm_index(args.cache) if args.cache.exists() else ({}, {}, None)

    rows = []
    for r in forecast_rows(args.forecasts):
        sym, t = r["symbol"], r["decided"]
        if sym in ETFS or sym not in starts or t - dt.timedelta(hours=72) < starts[sym]:
            continue
        at = story_at[sym]
        hi = bisect.bisect_right(at, t)
        assert hi == 0 or at[hi - 1] <= t
        n24 = hi - bisect.bisect_right(at, t - dt.timedelta(hours=24))
        n72 = hi - bisect.bisect_right(at, t - dt.timedelta(hours=72))
        rated = bisect.bisect_right(ratings[sym], t) - bisect.bisect_right(ratings[sym], t - dt.timedelta(hours=72))
        spot, iv, h = float(r["spot"]), float(r["atm_iv"]), float(r["horizon_years"])
        day = t.date()
        earn = any(-5 <= (d - day).days <= h * 365 for d in rel.get(sym, []))
        nxt = r["next"]
        llm = (llm_features(sym, t, effects, tone)
               if span and span[0] <= t - dt.timedelta(hours=72) and t <= span[1] else None)
        r1 = math.log(float(nxt["spot"]) / spot) if nxt else None
        rows.append(dict(llm=llm,
            z7=math.log(float(r["realized"]) / spot) / (iv * math.sqrt(h)),
            z1=r1 / (iv * math.sqrt((nxt["at"] - r["at"]).total_seconds() / YEAR)) if nxt else None,
            symbol=sym, day=day, n24=n24, n72=n72, rated=rated > 0, earnings=earn,
            m7=math.log(float(r["realized"]) / spot) ** 2, i7=iv * iv * h,
            m1=math.log(float(nxt["spot"]) / spot) ** 2 if nxt else None,
            i1=iv * iv * (nxt["at"] - r["at"]).total_seconds() / YEAR if nxt else None,
            model=float(r["model_crps"]), null=float(r["null_crps"]),
        ))

    # Within-symbol scaling. ponytail: full-sample per-symbol means — fine for
    # bucketing a descriptive table, look-ahead if it ever reached a decision.
    by = collections.defaultdict(list)
    for r in rows:
        by[r["symbol"]].append(r)
    for rs in by.values():
        k7 = sum(r["m7"] for r in rs) / sum(r["i7"] for r in rs)
        one = [r for r in rs if r["m1"] is not None]
        k1 = sum(r["m1"] for r in one) / sum(r["i1"] for r in one) if one else 1.0
        rate = sum(r["n24"] for r in rs) / len(rs)  # mean trailing-24h stories at a forecast bar
        for r in rs:
            r["i7"] *= k7
            if r["m1"] is not None:
                r["i1"] *= k1
            r["intensity"] = r["n24"] / rate if rate else 0.0

    def table(title, groups):
        rnd = random.Random(1)
        print(f"\n{title}")
        print(f"  {'bucket':28}{'rows':>7}{'wks':>5}  {'var7 [95%]':>22}  {'var1 [95%]':>22}  {'skill [95%]':>26}")
        for name, rs in groups:
            if len(rs) < 30:
                print(f"  {name:28}{len(rs):7}  (too few)")
                continue
            v7 = weekly_interval(rs, lambda r: r["m7"], lambda r: r["i7"], rnd)
            one = [r for r in rs if r["m1"] is not None]
            v1 = weekly_interval(one, lambda r: r["m1"], lambda r: r["i1"], rnd)
            sk = weekly_interval(rs, lambda r: r["null"] - r["model"], lambda r: r["null"], rnd)
            f = lambda p, lo, hi, _w, d=2: f"{p:.{d}f} [{lo:.{d}f}, {hi:.{d}f}]"
            print(f"  {name:28}{len(rs):7}{v7[3]:5}  {f(*v7):>22}  {f(*v1):>22}  {f(*sk, d=4):>26}")

    quiet = [r for r in rows if not r["earnings"]]
    print(f"forecasts: {args.forecasts}  rows with complete news: {len(rows)}  "
          f"symbols: {len(by)}  ex-earnings: {len(quiet)}")
    table("all rows, by earnings", [("release within -5d..horizon", [r for r in rows if r["earnings"]]),
                                    ("no release", quiet)])
    for label, key in (("stories in prior 24h", "n24"), ("stories in prior 72h", "n72")):
        table(f"ex-earnings, {label}", [
            ("none", [r for r in quiet if r[key] == 0]),
            ("1-2", [r for r in quiet if 1 <= r[key] <= 2]),
            ("3-9", [r for r in quiet if 3 <= r[key] <= 9]),
            ("10+", [r for r in quiet if r[key] >= 10]),
        ])
    table("ex-earnings, 24h stories vs the symbol's mean", [
        ("quiet (<0.5x)", [r for r in quiet if r["intensity"] < 0.5]),
        ("normal (0.5-2x)", [r for r in quiet if 0.5 <= r["intensity"] < 2]),
        ("busy (2-4x)", [r for r in quiet if 2 <= r["intensity"] < 4]),
        ("very busy (4x+)", [r for r in quiet if r["intensity"] >= 4]),
    ])
    table("ex-earnings, analyst rating change (Briefing) in prior 72h", [
        ("yes", [r for r in quiet if r["rated"]]), ("no", [r for r in quiet if not r["rated"]])])

    scoredq = [r for r in quiet if r["llm"]]
    if not scoredq:
        return
    print(f"\n=== LLM features: {len(scoredq)} ex-earnings rows scored {span[0]:%Y-%m-%d}..{span[1]:%Y-%m-%d} ===")
    L = lambda k: (lambda r: r["llm"][k])
    table("ex-earnings, max materiality of fresh news in prior 72h", [
        (f"{m}", [r for r in scoredq if L("mat")(r) == m]) for m in range(4)])
    table("ex-earnings, LLM vol flag in prior 72h", [
        ("elevated/high", [r for r in scoredq if L("volflag")(r)]), ("none", [r for r in scoredq if not L("volflag")(r)])])
    table("ex-earnings, material read-through from another symbol's news, 72h", [
        ("yes", [r for r in scoredq if L("cross")(r)]), ("no", [r for r in scoredq if not L("cross")(r)])])

    def direction(title, key):
        rnd = random.Random(1)
        print(f"\n{title}")
        print(f"  {'bucket':28}{'rows':>7}{'wks':>5}  {'up 7d [95%]':>22}  {'z1 [95%]':>24}  {'z7 [95%]':>24}")
        get = L(key)
        for name, rs in (("negative", [r for r in scoredq if get(r) is not None and get(r) < -LEAN]),
                         ("neutral", [r for r in scoredq if get(r) is not None and abs(get(r)) <= LEAN]),
                         ("positive", [r for r in scoredq if get(r) is not None and get(r) > LEAN]),
                         ("no reading", [r for r in scoredq if get(r) is None])):
            if len(rs) < 30:
                print(f"  {name:28}{len(rs):7}  (too few)")
                continue
            one = [r for r in rs if r["z1"] is not None]
            up = weekly_interval(rs, lambda r: float(r["z7"] > 0), lambda r: 1.0, rnd)
            z1 = weekly_interval(one, lambda r: r["z1"], lambda r: 1.0, rnd)
            z7 = weekly_interval(rs, lambda r: r["z7"], lambda r: 1.0, rnd)
            f = lambda p, lo, hi, _w, d=2: f"{p:+.{d}f} [{lo:+.{d}f}, {hi:+.{d}f}]"
            print(f"  {name:28}{len(rs):7}{z7[3]:5}  {f(*up):>22}  {f(*z1):>24}  {f(*z7):>24}")

    direction("ex-earnings, headline sentiment (materiality-weighted, 24h half-life)", "sent")
    direction("ex-earnings, LLM direction (same weighting)", "dirn")
    direction("ex-earnings, LLM per-symbol tone (latest reading in 72h)", "tone")


if __name__ == "__main__":
    main()
