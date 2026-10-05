#!/usr/bin/env python3
"""A systematic look at the LLM headline scores, written to `out/news_study.json`
for ib-option-viz's News Study page. Every hypothesis and grid here was fixed
before the run (see the plan in the page's Method section); anything added
after looking is marked `post_hoc`.

Sections
  A  coverage   where the headline record is complete, and the earlier
                findings re-run inside it
  B  kinds      fresh news by type, materiality, vol flag, horizon, read-through,
                arrival, attention and "priced in", against moves and variance
  C  macro      scheduled releases (FOMC, CPI, PPI, payrolls, GDP, PCE, retail
                sales): variance, the intraday profile, the ATM IV path
  D  sentiment  the decayed indicator re-cut by period, clock, weights, signal,
                optimism balancing and transform
  E  forecaster offline: does a news term improve the vol or drift model?
  F  day type   does morning news make a trend day?

Method, shared by every section:

**Coverage.** IB returns only the newest 255 headlines per provider and
symbol, so a heavy name's record is complete only from `news_profile.
complete_from` (NVDA 09-24, AAPL 09-10). Before that, "no news" may be a
headline never captured. A row counts only when its look-back sits after that
point and inside a scored segment; a score is read once its window has closed.

**Intervals** resample whole weeks (a 7-day horizon spans one, and stocks move
together within a day). **Searching** many cells finds some by chance, so each
searched family carries a placebo: the same statistics with the news shuffled
across symbols within each day (or, for macro releases, moved to random
non-release days), 200 times. A cell is a *finding* only when its interval
excludes the null and its |t| beats the placebo's 95th-percentile best |t|.

    python3 scripts/news_study.py [--db snapshot] [--cache news-llm.sqlite3] [--draws 200]
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
import subprocess
from pathlib import Path
from zoneinfo import ZoneInfo

from news_audit import LEAN, interval, panel, release_days
from news_explore import forecast_rows
from news_profile import complete_from, load
from news_scores import CACHE, _calls, scored, segments, sentiment_index
from option_premium import ETFS, default_db, weekly_interval

ROOT = Path(__file__).resolve().parent.parent
NY = ZoneInfo("America/New_York")
H = dt.timedelta(hours=1)
DAY = dt.timedelta(days=1)
MIN_N, MIN_WEEKS = 30, 3
VOL = ("none", "elevated", "high")


# ------------------------------------------------------------------ statistics

def monday(d: dt.date) -> dt.date:
    return d - dt.timedelta(days=d.weekday())


def stat(rows, num, den=None, null=0.0, rnd=None) -> dict:
    """Ratio of sums (a mean when `den` is None) with its weekly-block 95 %
    interval and a cluster-robust t against `null` (clusters = weeks). Rows
    need `day`; a row whose num is None is left out."""
    den = den or (lambda r: 1.0)
    rs = [r for r in rows if num(r) is not None]
    out = dict(n=len(rs), weeks=len({monday(r["day"]) for r in rs}), null=null)
    if out["n"] < MIN_N or out["weeks"] < MIN_WEEKS:
        out.update(point=(sum(map(num, rs)) / sum(map(den, rs))) if rs and sum(map(den, rs)) else None,
                   lo=None, hi=None, t=None)
        return out
    p, lo, hi, _ = weekly_interval(rs, num, den, rnd or random.Random(1))
    out.update(point=p, lo=lo, hi=hi, t=cluster_t(rs, num, den, null))
    return out


def cluster_t(rs, num, den, null) -> float | None:
    """(R - null) / SE with R the ratio of sums and SE clustered by week."""
    sd = sum(map(den, rs))
    r = sum(map(num, rs)) / sd
    by = collections.defaultdict(float)
    for x in rs:
        by[monday(x["day"])] += num(x) - r * den(x)
    g = len(by)
    se = math.sqrt(g / (g - 1) * sum(e * e for e in by.values())) / abs(sd) if g > 1 else 0.0
    return (r - null) / se if se else None


def sgn(x: float | None, lean: float = 0.0) -> int:
    return 0 if x is None else (x > lean) - (x < -lean)


def placebo_cut(maxes: list[float]) -> float:
    s = sorted(maxes)
    return s[int(0.95 * len(s))] if s else float("inf")


def judge(cells: list[dict], cut: float) -> None:
    """Stamp each cell with its verdict: the interval must exclude the null
    and |t| must beat the placebo's 95th-percentile best."""
    for c in cells:
        if c.get("lo") is None:
            c["verdict"] = "too few"
        elif c["lo"] <= c["null"] <= c["hi"]:
            c["verdict"] = "within noise"
        else:
            c["verdict"] = "finding" if c["t"] is not None and abs(c["t"]) >= cut else "chance (search)"


# ------------------------------------------------------------------ data

class Data:
    """Everything the sections read, loaded once."""

    def __init__(self, db_path: Path, cache: Path):
        self.db_path, self.cache = db_path, cache
        self.db = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
        self.items = scored(cache)
        self.segs = segments(self.items)
        self.rel = release_days(self.db)
        heads = collections.defaultdict(list)
        for r in load(self.db):
            heads[r["symbol"]].append(r)
        self.heads = heads
        self.start = {s: complete_from(rs) or min(r["at"] for r in rs) for s, rs in heads.items()}

        self.bars = collections.defaultdict(list)  # RTH 5-min trades: (t, o, h, l, c)
        for sym, ts, o, h, l, c in self.db.execute(
            """SELECT c.symbol, ts, open, high, low, close FROM bars b JOIN contracts c ON c.id = b.contract_id
               WHERE c.kind = 'stock' AND b.bar_size = '5min' AND b.data_type = 'trades' ORDER BY c.symbol, ts"""):
            self.bars[sym].append((dt.datetime.fromisoformat(ts), o, h, l, c))
        self.close, self.open = collections.defaultdict(dict), collections.defaultdict(dict)
        for sym, bs in self.bars.items():
            for t, o, _h, _l, c in bs:
                d = t.astimezone(NY).date()
                self.open[sym].setdefault(d, o)
                self.close[sym][d] = c
        self.sessions = sorted(self.close["SPY"])
        self.sidx = {d: i for i, d in enumerate(self.sessions)}
        self.symbols = sorted(self.bars)
        self.singles = [s for s in self.symbols if s not in ETFS]

        # Daily close-to-close log returns: single names net of SPY, in the
        # symbol's own sd. ponytail: full-sample sd, descriptive only.
        self.ret, self.sd = {}, {}
        spy = self.close["SPY"]
        for sym in self.symbols:
            cl, out = self.close[sym], {}
            for a, b in zip(self.sessions, self.sessions[1:]):
                if a in cl and b in cl and a in spy and b in spy:
                    out[b] = math.log(cl[b] / cl[a]) - (0 if sym in ETFS else math.log(spy[b] / spy[a]))
            self.ret[sym] = out
            self.sd[sym] = math.sqrt(sum(x * x for x in out.values()) / len(out))

        # Every LLM effect, own-tagged or read through, repeats flagged.
        self.effects = collections.defaultdict(list)
        for it in self.items:
            for e in it["effects"]:
                self.effects[e["symbol"]].append(dict(
                    e, at=it["available_at"], type=it["event_type"], repeat=it["is_repeat"],
                    cross=e["symbol"] not in it["symbols"]))
        for v in self.effects.values():
            v.sort(key=lambda e: e["at"])

    # -------------------------------------------------------------- helpers

    def known(self, sym: str, t: dt.datetime, back: dt.timedelta) -> bool:
        """The record for `sym` is complete over [t - back, t] and scored."""
        s = self.start.get(sym)
        return s is not None and s <= t - back and any(a <= t - back and t <= b for a, b in self.segs)

    def scored_span(self, t: dt.datetime, back: dt.timedelta) -> bool:
        return any(a <= t - back and t <= b for a, b in self.segs)

    def near_release(self, sym: str, d: dt.date, before: int, after: int) -> bool:
        return any(-before <= (x - d).days <= after for x in self.rel.get(sym, ()))

    def fresh(self, sym: str, lo: dt.datetime, hi: dt.datetime, floor: int = 1) -> list[dict]:
        """Non-repeat effects of materiality >= floor available in (lo, hi]."""
        es = self.effects.get(sym, [])
        i = bisect.bisect_right([e["at"] for e in es], lo)
        out = []
        for e in es[i:]:
            if e["at"] > hi:
                break
            if not e["repeat"] and e["materiality"] >= floor:
                out.append(e)
        return out

    def close_at(self, d: dt.date) -> dt.datetime:
        return dt.datetime(d.year, d.month, d.day, 16, tzinfo=NY).astimezone(dt.timezone.utc)

    def news_state(self, sym: str, t: dt.datetime) -> str:
        now = self.fresh(sym, t - DAY, t)
        return "none" if not now else "flagged" if any(e["vol"] != "none" for e in now) else "unflagged"


def forecasts(data: Data, path: Path) -> list[dict]:
    """`news_explore.forecast_rows` with floats parsed and the 7-day and
    next-session squared moves against ATM IV's implied variance."""
    out = []
    for r in forecast_rows(path):
        spot, iv, h = float(r["spot"]), float(r["atm_iv"] or 0), float(r["horizon_years"])
        if not iv or not r["realized"]:
            continue
        nxt = r["next"]
        y1 = (nxt["at"] - r["at"]).total_seconds() / (365 * 86400) if nxt else None
        out.append(dict(
            symbol=r["symbol"], at=r["at"], decided=r["decided"], day=r["decided"].date(), spot=spot, iv=iv,
            h=h, realized=float(r["realized"]), m7=math.log(float(r["realized"]) / spot) ** 2, i7=iv * iv * h,
            m1=math.log(float(nxt["spot"]) / spot) ** 2 if nxt else None, i1=iv * iv * y1 if nxt else None,
            iv_next=float(nxt["atm_iv"]) if nxt and nxt["atm_iv"] else None,
            q25=float(r["q25"]), q50=float(r["q50"]), q75=float(r["q75"]),
            model=float(r["model_crps"]), null=float(r["null_crps"]), expiry=r["expiry"]))
    return out


def within_symbol(rows: list[dict], m: str, i: str) -> None:
    """Scale each row's implied variance by its symbol's pooled realized /
    implied, so 1.00 is a normal move for that name (ETFs move less than
    implied in every bucket and must not pose as an effect)."""
    by = collections.defaultdict(list)
    for r in rows:
        if r[m] is not None:
            by[r["symbol"]].append(r)
    for rs in by.values():
        k = sum(r[m] for r in rs) / sum(r[i] for r in rs)
        for r in rs:
            r[i + "_rel"] = r[i] * k


# ------------------------------------------------------------------ A

def coverage(data: Data) -> dict:
    sent = collections.defaultdict(collections.Counter)
    for _end, items, _raw in _calls(data.cache):
        for it in items:
            d = dt.datetime.fromisoformat(it["at"]).astimezone(NY).date()
            for s in it["symbols"]:
                sent[s][monday(d).isoformat()] += 1
    a, b = data.segs[0][0], data.segs[-1][1]
    last = max(d for d in data.sessions if data.close_at(d) <= b)
    rows = []
    for sym in data.symbols:
        cf = data.start.get(sym)
        eff = max(cf, a) if cf else None
        usable = [d for d in data.sessions if eff and eff <= data.close_at(d) <= b and d in data.close[sym]]
        rows.append(dict(symbol=sym, etf=sym in ETFS, complete_from=cf.isoformat() if cf else None,
                         usable_sessions=len(usable), sessions=sum(a <= data.close_at(d) <= b for d in data.sessions
                                                                   if d in data.close[sym]),
                         headlines_by_week=dict(sorted(sent[sym].items()))))
    return dict(scored_from=a.isoformat(), scored_to=b.isoformat(), last_session=last.isoformat(), symbols=rows)


def event_panel(data: Data, masked: bool) -> list[dict]:
    """`news_audit.panel` (window move, rest / d1 / d5 in sd units) for single
    names, ex releases, with same-slot variance ratios recomputed against the
    quiet rows of *this* population — masked or not."""
    rows = [r for r in data.panel if not r["etf"] and not r["earnings"]
            and (not masked or data.known(r["symbol"], r["t"], DAY))]
    for h in ("pre", "rest", "d1", "d5"):
        base = collections.defaultdict(list)
        for r in rows:
            if r["quiet"] and r["z"][h] is not None:
                base[r["slot"]].append(r["z"][h] ** 2)
        mean = {k: sum(v) / len(v) for k, v in base.items()}
        for r in rows:
            r[h + "_v"] = r["z"][h] ** 2 / mean[r["slot"]] if r["z"][h] is not None and mean.get(r["slot"]) else None
    return rows


def recheck(data: Data, fc: list[dict]) -> dict:
    out = []
    for masked in (False, True):
        rows = event_panel(data, masked)
        fresh = [r for r in rows if r["mat"] >= 1]
        called = [dict(r, s=sgn(r["dirn"], LEAN)) for r in fresh if sgn(r["dirn"], LEAN)]
        if not masked:  # reproduce news_audit before trusting anything new
            ref = interval(called, lambda r: r["s"] * r["z"]["pre"] if r["z"]["pre"] is not None else None)
            mine = sum(r["s"] * r["z"]["pre"] for r in called if r["z"]["pre"] is not None) / ref[3]
            assert abs(ref[0] - mine) < 1e-12, (ref, mine)
            for r in rows:
                assert r["rest_var"] is None or abs(r["rest_var"] - r["rest_v"]) < 1e-9
        tag = "masked" if masked else "unmasked"
        flagged = [r for r in fresh if r["vol"] >= 1]
        for label, rs, f, null in (
            ("window move in the LLM's direction (sd)", called, lambda r: r["s"] * r["z"]["pre"] if r["z"]["pre"] is not None else None, 0.0),
            ("next close in the LLM's direction (sd)", called, lambda r: r["s"] * r["z"]["d1"] if r["z"]["d1"] is not None else None, 0.0),
            ("rest-of-session variance after vol-flagged news (x quiet)", flagged, lambda r: r["rest_v"], 1.0),
            ("variance to the fifth close after fresh news (x quiet)", fresh, lambda r: r["d5_v"], 1.0),
        ):
            out.append(dict(stat(rs, f, null=null), label=label, population=tag))
        frs = [dict(r, state=data.news_state(r["symbol"], r["decided"])) for r in fc
               if r["symbol"] not in ETFS and not data.near_release(r["symbol"], r["day"], 3, 7)
               and (data.known(r["symbol"], r["decided"], DAY) if masked else data.scored_span(r["decided"], DAY))]
        within_symbol(frs, "m7", "i7")
        for state in ("none", "unflagged", "flagged"):
            out.append(dict(stat([r for r in frs if r["state"] == state], lambda r: r["m7"], lambda r: r["i7_rel"], null=1.0),
                            label=f"7-day realized / implied, news {state} (x symbol's own)", population=tag))
    return dict(rows=out)


# ------------------------------------------------------------------ main

def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--db", default=None, type=Path)
    ap.add_argument("--cache", default=CACHE, type=Path)
    ap.add_argument("--forecasts", default=ROOT / "out/vol_timing_all/forecasts.csv", type=Path)
    ap.add_argument("--draws", default=200, type=int)
    ap.add_argument("--out", default=ROOT / "out/news_study.json", type=Path)
    args = ap.parse_args()
    data = Data(args.db or Path(default_db()), args.cache)
    data.panel = panel(args.cache, data.db)
    fc = forecasts(data, args.forecasts)

    sections = dict(coverage=coverage(data), recheck=recheck(data, fc))
    commit = subprocess.run(["git", "rev-parse", "--short", "HEAD"], cwd=ROOT, capture_output=True, text=True).stdout.strip()
    report = dict(provenance=dict(
        snapshot=str(data.db_path), cache=str(args.cache), forecasts=str(args.forecasts), commit=commit,
        generated_at=dt.datetime.now(dt.timezone.utc).isoformat(timespec="seconds"), draws=args.draws),
        sections=sections)
    args.out.parent.mkdir(exist_ok=True)
    args.out.write_text(json.dumps(report, indent=1, default=str))
    print(f"wrote {args.out}")


if __name__ == "__main__":
    main()
