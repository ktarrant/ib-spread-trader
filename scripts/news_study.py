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
# An interval or a t needs this many rows and week clusters to mean anything;
# a 3-week bootstrap is three events reshuffled.
MIN_N, MIN_WEEKS = 30, 5
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


def judge(cells: list[dict], cut: float | None) -> None:
    """Stamp each cell with its verdict. A searched cell (`cut` from its
    family's placebo) must exclude the null *and* beat the cut; a single
    pre-registered test (`cut` 0) needs only the interval; a cell no placebo
    guards (`cut` None) is at most "unguarded"."""
    for c in cells:
        if c.get("lo") is None or (cut and c["weeks"] < MIN_WEEKS):
            c["verdict"] = "too few"
        elif c["lo"] <= c["null"] <= c["hi"]:
            c["verdict"] = "within noise"
        elif cut is None:
            c["verdict"] = "unguarded"
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
        self.ret, self.ret_raw, self.sd = {}, {}, {}
        spy = self.close["SPY"]
        for sym in self.symbols:
            cl, out, raw = self.close[sym], {}, {}
            for a, b in zip(self.sessions, self.sessions[1:]):
                if a in cl and b in cl and a in spy and b in spy:
                    raw[b] = math.log(cl[b] / cl[a])
                    out[b] = raw[b] - (0 if sym in ETFS else math.log(spy[b] / spy[a]))
            self.ret[sym], self.ret_raw[sym] = out, raw
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
            # Same chain only: the feature chain can roll, which steps ATM IV.
            iv_next=float(nxt["atm_iv"]) if nxt and nxt["atm_iv"] and nxt["expiry"] == r["expiry"] else None,
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
    rows = [dict(r) for r in data.panel if not r["etf"] and not r["earnings"]
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


# ------------------------------------------------------------------ B

# Target: (description, null). Directional targets are signed by the LLM's
# direction; `rev` is the next close signed by the window's own move.
KIND_TARGETS = {
    "pre": ("window move in the LLM's direction (sd)", 0.0),
    "rest": ("rest of session in the LLM's direction (sd)", 0.0),
    "d1": ("next close in the LLM's direction (sd)", 0.0),
    "d5": ("fifth close in the LLM's direction (sd/√5)", 0.0),
    "rest_v": ("rest-of-session variance (x same-slot quiet)", 1.0),
    "d1_v": ("next-close variance (x same-slot quiet)", 1.0),
    "d5_v": ("fifth-close variance (x same-slot quiet)", 1.0),
    "rv1": ("next-session realized / implied (x symbol's own)", 1.0),
    "div": ("ATM IV change to the next session (log)", 0.0),
    "rev": ("next close against the window's move (sd; < 0 = reversal)", 0.0),
}
KIND_AXES = ("type", "materiality", "vol", "horizon", "source", "arrival", "attention", "tone_vs_direction")
DIRECTIONAL = ("pre", "rest", "d1", "d5")


def kind_rows(data: Data, fc: list[dict]) -> list[dict]:
    """One row per (symbol, window end) with fresh material news, labelled by
    its most material effect, inside complete coverage."""
    pool = [r for r in fc if r["symbol"] not in ETFS and not data.near_release(r["symbol"], r["day"], 3, 7)
            and data.known(r["symbol"], r["decided"], DAY)]
    within_symbol(pool, "m1", "i1")
    by_sym = collections.defaultdict(list)
    for r in sorted(pool, key=lambda r: r["decided"]):
        by_sym[r["symbol"]].append(r)
    dec = {s: [r["decided"] for r in rs] for s, rs in by_sym.items()}

    out = []
    for r in event_panel(data, masked=True):
        if r["mat"] < 1:
            continue
        sym, t = r["symbol"], r["t"]
        now = data.fresh(sym, t - dt.timedelta(microseconds=1), t)
        assert now and all(e["at"] <= t for e in now)
        lead = max(now, key=lambda e: (e["materiality"], abs(e["direction"])))
        att = sum(e["materiality"] >= 1 for e in data.effects[sym] if t - DAY < e["at"] <= t)
        s, tone = sgn(r["dirn"], LEAN), sgn(r["sent"], LEAN)
        z = r["z"]
        i = bisect.bisect_left(dec.get(sym, []), t)
        f = by_sym[sym][i] if i < len(dec.get(sym, [])) and dec[sym][i] - t < DAY else None
        out.append(dict(
            symbol=sym, t=t, day=r["day"],
            labels=dict(
                type=lead["type"], materiality=str(r["mat"]), vol=VOL[r["vol"]], horizon=lead["horizon"],
                source="read-through" if all(e["cross"] for e in now) else "own",
                arrival="overnight / open" if r["slot"] == "open" else "intraday",
                attention="1" if att <= 1 else "2-4" if att <= 4 else "5+",
                tone_vs_direction="no tone" if not tone else "aligned" if tone == s else "priced in",
                dir_sign=s),
            x=dict(
                pre=z["pre"], rest=z["rest"], d1=z["d1"], d5=z["d5"],
                rest_v=r["rest_v"], d1_v=r["d1_v"], d5_v=r["d5_v"],
                rv1=f["m1"] / f["i1_rel"] if f and f["m1"] is not None else None,
                div=math.log(f["iv_next"] / f["iv"]) if f and f["iv_next"] else None,
                rev=sgn(z["pre"]) * z["d1"] if z["pre"] and z["d1"] is not None else None)))
    return out


def kind_value(row: dict, labels: dict, target: str) -> float | None:
    x = row["x"][target]
    if x is None:
        return None
    if target in DIRECTIONAL:
        return labels["dir_sign"] * x if labels["dir_sign"] else None
    return x


def kind_t(rows: list[dict], labels: list[dict]) -> tuple[dict, dict]:
    """Cluster t of every (axis, level, target) cell, against its null and
    against the pooled mean of all news. One pass per target."""
    t_abs, t_diff = {}, {}
    for target, (_desc, null) in KIND_TARGETS.items():
        xs = [(r["day"], lab, kind_value(r, lab, target)) for r, lab in zip(rows, labels)]
        xs = [x for x in xs if x[2] is not None]
        if not xs:
            continue
        pooled = sum(x[2] for x in xs) / len(xs)
        acc = collections.defaultdict(lambda: collections.defaultdict(lambda: [0, 0.0]))
        for day, lab, x in xs:
            w = monday(day)
            for axis in KIND_AXES:
                a = acc[axis, lab[axis]][w]
                a[0] += 1
                a[1] += x
        for (axis, level), weeks in acc.items():
            n = sum(a[0] for a in weeks.values())
            g = len(weeks)
            if n < MIN_N or g < MIN_WEEKS:
                continue
            m = sum(a[1] for a in weeks.values()) / n
            se = math.sqrt(g / (g - 1) * sum((a[1] - a[0] * m) ** 2 for a in weeks.values())) / n
            if se:
                t_abs[axis, level, target] = (m - null) / se
                t_diff[axis, level, target] = (m - pooled) / se
    return t_abs, t_diff


def kinds(data: Data, fc: list[dict], draws: int) -> dict:
    rows = kind_rows(data, fc)
    labels = [r["labels"] for r in rows]
    by_day = collections.defaultdict(list)
    for i, r in enumerate(rows):
        by_day[r["day"]].append(i)
    rnd = random.Random(7)
    max_dir, max_diff = [], []
    for _ in range(draws):
        shuf = list(labels)
        for idx in by_day.values():
            perm = idx[:]
            rnd.shuffle(perm)
            for a, b in zip(idx, perm):
                shuf[a] = labels[b]
        ta, td = kind_t(rows, shuf)
        max_dir.append(max((abs(v) for (_a, _l, tg), v in ta.items() if tg in DIRECTIONAL), default=0.0))
        max_diff.append(max((abs(v) for v in td.values()), default=0.0))
    cut_dir, cut_diff = placebo_cut(max_dir), placebo_cut(max_diff)

    cells = []
    for target, (desc, null) in KIND_TARGETS.items():
        vals = [kind_value(r, r["labels"], target) for r in rows]
        have = [(r, v) for r, v in zip(rows, vals) if v is not None]
        pooled = sum(v for _r, v in have) / len(have) if have else 0.0
        all_cell = dict(stat([dict(day=r["day"], v=v) for r, v in have], lambda x: x["v"], null=null),
                        axis="all", level="all news", target=target, description=desc, kind="absolute")
        cells.append(all_cell)
        for axis in KIND_AXES:
            for level in sorted({r["labels"][axis] for r, _v in have}):
                sub = [dict(day=r["day"], v=v) for r, v in have if r["labels"][axis] == level]
                cells.append(dict(stat(sub, lambda x: x["v"], null=null), axis=axis, level=level,
                                  target=target, description=desc, kind="absolute"))
                cells.append(dict(stat(sub, lambda x: x["v"] - pooled, null=0.0), axis=axis, level=level,
                                  target=target, description=desc, kind="vs all news"))
    for c in cells:
        if c["axis"] == "all":
            judge([c], 0.0)  # one pre-registered test per target
        elif c["kind"] == "vs all news":
            judge([c], cut_diff)
        elif c["target"] in DIRECTIONAL:
            judge([c], cut_dir)  # the label shuffle randomizes the LLM's direction
        else:  # a row's own variance or move: shuffling labels cannot null it
            judge([c], None)
    return dict(events=len(rows), weeks=len({monday(r["day"]) for r in rows}),
                targets={k: dict(description=d, null=n) for k, (d, n) in KIND_TARGETS.items()},
                placebo=dict(directional_t95=cut_dir, vs_all_news_t95=cut_diff, draws=draws), cells=cells)


# ------------------------------------------------------------------ C

MACRO = ("fomc", "cpi", "ppi", "jobs_report", "gdp", "pce", "retail_sales")


def calendar(data: Data) -> list[dict]:
    """Scheduled releases inside the stock tape, each on the session it
    lands in (08:30 releases move that session's open)."""
    lo, hi = data.sessions[0], data.sessions[-1]
    out = []
    for kind, ts in data.db.execute(
            f"SELECT kind, event_time FROM events WHERE kind IN ({','.join('?' * len(MACRO))}) ORDER BY event_time", MACRO):
        t = dt.datetime.fromisoformat(ts)
        d = t.astimezone(NY).date()
        if lo < d <= hi and d in data.sidx:
            out.append(dict(kind=kind, at=t, day=d))
    return out


def placebo_pct(real: float, draws: list[float]) -> float:
    """Share of placebo draws at or below the real value (0.5 = typical)."""
    return sum(x <= real for x in draws) / len(draws)


def macro(data: Data, fc: list[dict], draws: int) -> dict:
    cal = calendar(data)
    rnd = random.Random(11)
    days_by_kind = collections.defaultdict(set)
    for e in cal:
        days_by_kind[e["kind"]].add(e["day"])
        days_by_kind["any release"].add(e["day"])
    days_by_kind["08:30 releases"] = {e["day"] for e in cal if e["kind"] != "fomc"}
    quiet_days = [d for d in data.sessions[1:] if d not in days_by_kind["any release"]]

    # C1: session variance (close to close, raw: a release moves the market,
    # not one name against it) over the symbol's own non-release mean.
    quiet_mean = {}
    for sym in data.symbols:
        base = [data.ret_raw[sym][d] ** 2 for d in quiet_days if d in data.ret_raw[sym]]
        quiet_mean[sym] = sum(base) / len(base)
    rows = [dict(symbol=sym, day=d, v=x * x / quiet_mean[sym]) for sym in data.symbols
            for d, x in data.ret_raw[sym].items()]
    day_sum = {scope: collections.defaultdict(lambda: [0.0, 0]) for scope in ("ETFs", "single names")}
    for r in rows:
        a = day_sum["ETFs" if r["symbol"] in ETFS else "single names"][r["day"]]
        a[0] += r["v"]
        a[1] += 1
    c1 = []
    for group in ("any release", "08:30 releases", *MACRO):
        days = days_by_kind.get(group, set())
        if not days:
            continue
        for scope in ("ETFs", "single names"):
            sub = [r for r in rows if r["day"] in days and (r["symbol"] in ETFS) == (scope == "ETFs")]
            mean_of = lambda ds: (lambda a: a[0] / a[1] if a[1] else 0.0)(
                [sum(day_sum[scope][d][0] for d in ds), sum(day_sum[scope][d][1] for d in ds)])
            plc = [mean_of(rnd.sample(quiet_days, len(days))) for _ in range(draws)]
            c = dict(stat(sub, lambda r: r["v"], null=1.0), group=group, scope=scope, days=len(days))
            c["placebo_pct"] = placebo_pct(c["point"], plc) if c["point"] is not None else None
            c1.append(c)
    fomc = [dict(day=d.isoformat(), **{s: data.ret_raw[s][d] ** 2 / quiet_mean[s] for s in ETFS if d in data.ret_raw[s]})
            for d in sorted(days_by_kind["fomc"])]

    # C2: the 5-minute |return| profile, ETFs pooled, by day group (bp), plus the opening gap.
    groups = dict(fomc=days_by_kind["fomc"], early=days_by_kind["08:30 releases"] - days_by_kind["fomc"],
                  normal=set(quiet_days))
    prof = {g: collections.defaultdict(list) for g in groups}
    gap = {g: [] for g in groups}
    for sym in ETFS:
        prev, last_day = None, None
        for t, o, _h, _l, c in data.bars[sym]:
            local = t.astimezone(NY)
            d = local.date()
            g = next((k for k, v in groups.items() if d in v), None)
            if d != last_day:
                if g and prev:
                    gap[g].append(abs(math.log(o / prev)) * 1e4)
                ref, last_day = o, d
            if g:
                prof[g][local.strftime("%H:%M")].append(abs(math.log(c / ref)) * 1e4)
            ref = prev = c
    slots = sorted(prof["normal"])
    c2 = dict(slots=slots, **{g: [sum(prof[g][s]) / len(prof[g][s]) if prof[g][s] else None for s in slots]
                              for g in groups},
              gap={g: sum(v) / len(v) if v else None for g, v in gap.items()},
              days={g: len(v) for g, v in groups.items()})
    # FOMC afternoon against random non-release afternoons, per ETF-day.
    aft = collections.defaultdict(float)
    for sym in ETFS:
        bs = data.bars[sym]
        for (t0, _o, _h, _l, c0), (t1, _o1, _h1, _l1, c1_) in zip(bs, bs[1:]):
            l1 = t1.astimezone(NY)
            if l1.date() == t0.astimezone(NY).date() and l1.strftime("%H:%M") >= "14:00":
                aft[l1.date()] += math.log(c1_ / c0) ** 2
    real = sum(aft[d] for d in days_by_kind["fomc"]) / len(days_by_kind["fomc"])
    plc = [sum(aft[d] for d in rnd.sample(quiet_days, len(days_by_kind["fomc"]))) / len(days_by_kind["fomc"])
           for _ in range(draws)]
    normal = sum(aft[d] for d in quiet_days) / len(quiet_days)
    c2["fomc_afternoon"] = dict(ratio=real / normal, placebo_pct=placebo_pct(real, plc))

    # C3: ETF ATM IV around releases: log change from the prior close, same chain only.
    eod = collections.defaultdict(dict)  # sym -> day -> (iv, expiry) at the session's last forecast bar
    for r in fc:
        if r["symbol"] in ETFS:
            eod[r["symbol"]][r["at"].astimezone(NY).date()] = (r["iv"], r["expiry"])

    def path(day, sym, k):
        i = data.sidx[day]
        if i - 1 < 0 or i + k >= len(data.sessions) or i + k < 0:
            return None
        base = eod[sym].get(data.sessions[i - 1])
        x = eod[sym].get(data.sessions[i + k])
        return math.log(x[0] / base[0]) if base and x and x[1] == base[1] else None

    c3 = {}
    ks = list(range(-5, 6))
    for group, days in (("fomc", days_by_kind["fomc"]), ("08:30 releases", days_by_kind["08:30 releases"] - days_by_kind["fomc"]),
                        ("non-release", set(quiet_days))):
        pts = {k: [p for d in days for s in ETFS if (p := path(d, s, k)) is not None] for k in ks}
        c3[group] = dict(k=ks, mean=[sum(v) / len(v) if v else None for v in pts.values()], n=[len(v) for v in pts.values()])
    same = lambda days: [p for d in days for s in ETFS if (p := path(d, s, 0)) is not None]
    for group in ("fomc", "08:30 releases"):
        days = days_by_kind[group] - (days_by_kind["fomc"] if group != "fomc" else set())
        real = same(days)
        plc = []
        for _ in range(draws):
            v = same(rnd.sample(quiet_days, len(days)))
            plc.append(sum(v) / len(v) if v else 0.0)
        c3[group]["release_day"] = dict(mean=sum(real) / len(real) if real else None, n=len(real),
                                        placebo_pct=placebo_pct(sum(real) / len(real), plc) if real else None)

    # C4 and C6: realized / implied when a release falls inside the horizon.
    times = sorted(e["at"] for e in cal)
    fomc_t = sorted(e["at"] for e in cal if e["kind"] == "fomc")
    inside = lambda ts, a, b: bisect.bisect_right(ts, b) > bisect.bisect_right(ts, a)
    pool = [dict(r) for r in fc if not data.near_release(r["symbol"], r["day"], 3, 7)]
    within_symbol(pool, "m7", "i7")
    within_symbol(pool, "m1", "i1")
    c4 = []
    for scope in ("ETFs", "single names"):
        sub = [r for r in pool if (r["symbol"] in ETFS) == (scope == "ETFs")]
        for label, ts in (("FOMC", fomc_t), ("any release", times)):
            for horizon, m, i, end in (("7 days", "m7", "i7_rel", lambda r: r["decided"] + dt.timedelta(days=7)),
                                       ("next session", "m1", "i1_rel", lambda r: r["decided"] + DAY)):
                for has in (True, False):
                    rs = [r for r in sub if r[m] is not None and inside(ts, r["decided"], end(r)) == has]
                    c4.append(dict(stat(rs, lambda r: r[m], lambda r: r[i], null=1.0), scope=scope, release=label,
                                   horizon=horizon, inside=has))
    judge(c4, 0.0)

    trades = []
    bt = ROOT / "out/skewed_condor_etf-all/backtest.json"
    if bt.exists():
        for tr in json.loads(bt.read_text()).get("trades", []):
            a, b = (dt.datetime.fromisoformat(tr[k].replace("Z", "+00:00")) for k in ("opened_at", "closed_at"))
            trades.append(dict(symbol=tr["symbol"], opened=a.isoformat(), closed=b.isoformat(), pnl=tr["pnl"],
                               fomc=inside(fomc_t, a, b), releases=bisect.bisect_right(times, b) - bisect.bisect_right(times, a)))

    # C5: market-wide LLM news per session against the next session.
    c5 = market_news(data, draws, rnd)
    return dict(calendar=[dict(kind=e["kind"], at=e["at"].isoformat(), day=e["day"].isoformat()) for e in cal],
                variance=c1, fomc_days=fomc, profile=c2, iv=c3, implied=c4, condor_trades=trades, market_news=c5)


def market_news(data: Data, draws: int, rnd: random.Random) -> dict:
    """Summed materiality of fresh news per session, averaged over the
    symbols whose record is complete that day (coverage grows through the
    sample, so a raw sum would trend), and the count of fresh `macro` items."""
    spy_mean = sum(x * x for x in data.ret_raw["SPY"].values()) / len(data.ret_raw["SPY"])
    rows = []
    for i, d in enumerate(data.sessions[1:-1], start=1):
        lo, hi = data.close_at(data.sessions[i - 1]), data.close_at(d)
        if not data.scored_span(hi, hi - lo):
            continue
        live = [s for s in data.heads if data.known(s, hi, hi - lo)]
        if not live:
            continue
        mat = sum(e["materiality"] for s in live for e in data.fresh(s, lo, hi))
        macro_n = sum(1 for it in data.items if lo < it["available_at"] <= hi and it["event_type"] == "macro"
                      and not it["is_repeat"])
        nxt = data.sessions[i + 1]
        disp = [data.ret[s][nxt] for s in data.singles if nxt in data.ret[s]]
        spy = data.ret_raw["SPY"]
        rows.append(dict(day=d, intensity=mat / len(live), macro=macro_n, live=len(live),
                         spy_next=spy[nxt] ** 2 / spy_mean if nxt in spy else None,
                         spy_same=spy[d] ** 2 / spy_mean if d in spy else None,
                         disp_next=statistics_sd(disp) if len(disp) > 3 else None))
    md = [r["disp_next"] for r in rows if r["disp_next"] is not None]
    mean_disp = sum(md) / len(md)
    for r in rows:
        r["disp_next"] = r["disp_next"] / mean_disp if r["disp_next"] is not None else None
    cells = []
    for feat in ("intensity", "macro"):
        xs = sorted(r[feat] for r in rows)
        hi_cut = xs[2 * len(xs) // 3]
        lo_cut = xs[len(xs) // 3]
        for target in ("spy_same", "spy_next", "disp_next"):
            top = [r for r in rows if r[feat] > hi_cut]
            bot = [r for r in rows if r[feat] <= lo_cut]
            mean = lambda rs: (lambda v: sum(v) / len(v) if v else None)([r[target] for r in rs if r[target] is not None])
            real = (mean(top) or 0) - (mean(bot) or 0)
            vals = [r[feat] for r in rows]
            plc = []
            for _ in range(draws):
                rnd.shuffle(vals)
                sh = [dict(r, f=v) for r, v in zip(rows, vals)]
                plc.append((mean([r for r in sh if r["f"] > hi_cut]) or 0) - (mean([r for r in sh if r["f"] <= lo_cut]) or 0))
            cells.append(dict(feature=feat, target=target, top=mean(top), bottom=mean(bot), diff=real,
                              n_top=len(top), n_bottom=len(bot), placebo_pct=placebo_pct(real, plc)))
    return dict(sessions=len(rows), cells=cells,
                series=[dict(day=r["day"].isoformat(), intensity=r["intensity"], macro=r["macro"], live=r["live"]) for r in rows])


def statistics_sd(xs: list[float]) -> float:
    m = sum(xs) / len(xs)
    return math.sqrt(sum((x - m) ** 2 for x in xs) / (len(xs) - 1))


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

    sections = dict(coverage=coverage(data), recheck=recheck(data, fc), kinds=kinds(data, fc, args.draws),
                    macro=macro(data, fc, args.draws))
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
