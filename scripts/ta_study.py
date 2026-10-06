#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = ["numpy", "pandas"]
# ///
"""Technical indicators on the 10-year 5-minute stock tape, written to
`out/ta_study.json` for ib-option-viz's TA Study page. The catalog
(`ta_indicators.CATALOG`), targets, split, contexts and pairs were fixed
before the first run; nothing here is tuned on what it finds.

Sections
  A  coverage      what the tape holds per symbol, and a check of the 5-minute
                   fold against IB's own daily bars
  B  catalog       every indicator and signal, with its textbook reading
  C  direction     does a signal's reading move the forward return its way?
                   daily (1/5/10 sessions, raw and SPY-hedged) and 15-minute
                   (to the close, to the next close)
  D  move size     does it forecast realized vol against trailing vol? plus
                   which trailing-vol estimator forecasts best (HAR-RV et al.)
                   and SPY's 10-year variance premium against VIX/VIX9D
  E  entry / exit  a triple barrier (+/-1 weekly sd, 5 sessions, first touch on
                   the 5-minute path) after each signal fires, and the path
  F  combinations  signal x context, confirmation pairs, consensus counts
  G  leads         every discovery finding, re-judged once on the holdout
  H  levels        support and resistance: do pivots, Fibonacci, the prior
                   session's range and volume profile, and the initial balance
                   reject price more often than a matched random level, alone
                   and with a confirming signal? (catalog v2, added after v1)

Method, shared by every section:

**Units.** Returns are log returns over trailing 20-session daily sd (no
full-sample sd: that would look ahead), entered at the next bar's open:
decide on N, fill on N+1. A statistic is a signal's *excess* over its
symbol's own mean in the same period, signed the way the textbook reads it,
so the decade's drift (the universe is today's winners) cannot pose as edge.

**Intervals** resample whole blocks: weeks for one-session and intraday
targets, months for five sessions and longer (a horizon must sit inside its
block). **Searching** many cells finds some by chance, so each family carries
a placebo: every signal circularly shifted in time by a random year or more
per symbol, 200 times, which keeps its base rate and persistence and breaks
its timing. A cell is a *finding* when its interval excludes zero and its |t|
beats the 95th percentile of the shuffles' best |t|.

**Holdout.** Discovery is 2016-09 to 2022; holdout 2023 onward. Leads are
picked mechanically from discovery findings and judged once on the holdout
as single pre-registered tests.

    uv run scripts/ta_study.py [--db snapshot] [--draws 200] [--out out/ta_study.json]
"""

from __future__ import annotations

import argparse
import bisect
import datetime as dt
import json
import math
import sqlite3
import subprocess
import sys
import time
import warnings
from functools import lru_cache
from pathlib import Path

import numpy as np
import pandas as pd
from numpy.lib.stride_tricks import sliding_window_view

import ta_indicators as ti
from news_audit import release_days
from news_study import MIN_N, MIN_WEEKS, crps_normal, judge, monday, placebo_cut, signed, stat
from option_premium import default_db

warnings.filterwarnings("ignore", category=RuntimeWarning)  # all-NaN slices are expected, and NaN is the answer

ROOT = Path(__file__).resolve().parent.parent
SPLIT = dt.date(2023, 1, 1)
MIN_SESSIONS = 300           # enough to warm a 200-day SMA and still have a year
CONTEXT_ONLY = ("HYG", "UUP", "USO", "GDX", "GLD", "TLT", "SLV")  # not equities: inputs, not targets
INDICES = ("VIX", "VIX3M", "VIX9D", "VVIX")
HORIZONS = (1, 5, 10)
PATH = 10
BARRIER = 5
BOOT = 1000
MIN_SHIFT = 252              # a placebo moves a signal at least a year
S5, S15 = 78, 26             # 5- and 15-minute slots in a regular session
GROUPS = ("all", "etf", "single")
CONTEXTS = ("sym_above_200", "spy_above_200", "adx_trending", "adx_flat", "vix_backwardation",
            "vix_top_third", "vix_bottom_third", "rv_top_third", "rv_bottom_third",
            "prevailing_up", "prevailing_down", "earnings_near")
CORE = {  # confirmation pairs: (label, bullish signal, bearish signal), crossed across families only
    "trend": (("macd_cross", "macd_cross_up", "macd_cross_dn"), ("supertrend", "st_flip_up", "st_flip_dn"),
              ("donchian20", "dc20_up", "dc20_dn"), ("di_cross", "di_cross_up", "di_cross_dn"),
              ("sar_flip", "sar_flip_up", "sar_flip_dn")),
    "oscillator": (("rsi14_cross", "rsi14_cross_up30", "rsi14_cross_dn70"), ("stoch_cross", "stoch_cross_up", "stoch_cross_dn"),
                   ("rsi2", "rsi2_lt10", "rsi2_gt90"), ("bollinger", "bb_below", "bb_above"), ("ibs", "ibs_lt20", "ibs_gt80")),
    "volume": (("obv_div", "obv_div_bull", "obv_div_bear"), ("cmf", "cmf_gt10", "cmf_lt10"), ("mfi", "mfi_lt20", "mfi_gt80")),
}
RECENT = 3                   # a pair agrees when both fired within this many sessions
WIN, BAND = 12, 0.25         # a level's outcome: 12 five-minute bars, barriers +/-0.25 daily sd from it
CONFLUENCE = 0.1             # another level within this many daily sd confirms
LEVEL_DRAWS = 60             # placebo level sets: the baseline and the search bar


def is_etf(sym: str) -> bool:
    return sym in ("SPY", "QQQ", "IWM", "DIA") or sym.startswith("XL")


def log(msg: str) -> None:
    print(msg, file=sys.stderr, flush=True)


# ------------------------------------------------------------------ array helpers

shift = ti.shift


def lead(x, k):
    out = np.full(len(x), np.nan)
    if 0 < k < len(x):
        out[:-k] = x[k:]
    return out


def first_valid(a):
    i = np.argmax(~np.isnan(a), axis=-1)
    return np.take_along_axis(a, i[..., None], -1)[..., 0]


def last_valid(a):
    i = a.shape[-1] - 1 - np.argmax(~np.isnan(a[..., ::-1]), axis=-1)
    return np.take_along_axis(a, i[..., None], -1)[..., 0]


def fold(g):
    """(..., k, 5) OHLCV sub-bars -> (..., 5): first open, extremes, last close, summed volume."""
    c = last_valid(g[..., 3])
    v = np.nansum(g[..., 4], -1)
    v[np.isnan(c)] = np.nan
    return np.stack([first_valid(g[..., 0]), np.nanmax(g[..., 1], -1), np.nanmin(g[..., 2], -1), c, v], -1)


def returns5(g):
    """Log returns of each 5-minute bar, the first from the session's open."""
    r = np.empty(g.shape[:2])
    r[:, 0] = np.log(g[:, 0, 3] / g[:, 0, 0])
    r[:, 1:] = np.diff(np.log(g[:, :, 3]), axis=1)
    return r


def quintile_codes(x, per_session: float):
    """+1 / -1 / 0 for the top / bottom fifth of a series' own trailing year
    of valid readings (the current one excluded), NaN before a full year."""
    out = np.full(len(x), np.nan)
    ok = np.flatnonzero(~np.isnan(x))
    w = max(20, int(round(252 * per_session)))
    if len(ok) <= w:
        return out
    v = pd.Series(x[ok])
    lo = v.rolling(w, min_periods=w).quantile(0.2).shift(1).to_numpy()
    hi = v.rolling(w, min_periods=w).quantile(0.8).shift(1).to_numpy()
    code = np.where(v.to_numpy() > hi, 1.0, np.where(v.to_numpy() < lo, -1.0, 0.0))
    code[np.isnan(hi)] = np.nan
    out[ok] = code
    return out


def r5(x):
    return None if x is None or not np.isfinite(x) else round(float(x), 5)


# ------------------------------------------------------------------ data

SQL = ("SELECT b.ts, b.open, b.high, b.low, b.close, b.volume FROM bars b JOIN contracts c ON c.id = b.contract_id "
       "WHERE c.kind = ? AND c.symbol = ? AND b.bar_size = '5min' AND b.data_type = 'trades' ORDER BY b.ts")


def tape(con, sym: str, kind: str = "stock"):
    """A symbol's regular-hours 5-minute bars as (sessions, (n, 78, OHLCV))."""
    rows = con.execute(SQL, (kind, sym)).fetchall()
    if not rows:
        return np.array([], dtype=object), np.empty((0, S5, 5))
    df = pd.DataFrame(rows, columns=["ts", "o", "h", "l", "c", "v"])
    t = pd.to_datetime(df["ts"], utc=True, format="ISO8601").dt.tz_convert("America/New_York")
    slot = ((t.dt.hour * 60 + t.dt.minute - 570) // 5).to_numpy()
    keep = (slot >= 0) & (slot < S5)
    days, inv = np.unique(np.array(t.dt.date)[keep], return_inverse=True)
    g = np.full((len(days), S5, 5), np.nan)
    g[inv, slot[keep]] = df[["o", "h", "l", "c", "v"]].to_numpy()[keep]
    return days, g


def whole(g) -> np.ndarray:
    """Sessions that start at 09:30 and run at least a half day. A listing
    day or a capture that began mid-session would misstate the open."""
    return ~np.isnan(g[:, 0, 3]) & ((~np.isnan(g[:, :, 3])).sum(1) >= 40)


class Blocks:
    """Week or month blocks over the calendar, each wholly in one period."""

    def __init__(self, sess: list[dt.date], unit: str):
        keys = [monday(d) if unit == "week" else d.replace(day=1) for d in sess]
        self.days = sorted(set(keys))
        ix = {k: i for i, k in enumerate(self.days)}
        self.id = np.array([ix[k] for k in keys])
        self.nb, self.unit = len(self.days), unit
        self.disc = np.array([k < SPLIT for k in self.days])
        self.year = np.array([k.year for k in self.days])


class Data:
    def __init__(self, db_path: Path):
        t0 = time.time()
        self.db_path = db_path
        self.con = con = sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)
        registered = [r[0] for r in con.execute("SELECT DISTINCT symbol FROM contracts WHERE kind = 'stock' ORDER BY symbol")]
        tapes = {s: tape(con, s) for s in registered}
        ok = {s: whole(g) for s, (_d, g) in tapes.items()}
        self.syms = [s for s in registered if s not in CONTEXT_ONLY and ok[s].sum() >= MIN_SESSIONS]
        self.sess = sorted(set().union(*(set(tapes[s][0][ok[s]]) for s in self.syms)))
        self.pos = {d: i for i, d in enumerate(self.sess)}
        n, m = len(self.sess), len(self.syms)
        self.n, self.m = n, m
        self.disc = np.array([d < SPLIT for d in self.sess])
        self.week, self.month = Blocks(self.sess, "week"), Blocks(self.sess, "month")
        self.etf = np.array([is_etf(s) for s in self.syms])
        self.groups = dict(all=np.ones(m, bool), etf=self.etf, single=~self.etf)
        self.D: dict[str, np.ndarray] = {}
        self.I: dict[str, np.ndarray] = {}
        self.idx: list[np.ndarray] = []
        self.lv: list[dict] = []
        log(f"loaded {len(registered)} registered stocks, universe {m}, {n} sessions ({time.time() - t0:.0f}s)")

        self.coverage_rows = self._coverage(registered, tapes, ok)
        for si, s in enumerate(self.syms):
            days, g = tapes[s]
            idx = np.array([self.pos[d] for d in days[ok[s]]])
            self.idx.append(idx)
            self._symbol(si, idx, g[ok[s]])
        self.spans = [(int(i[0]), int(i[-1]) + 1) for i in self.idx]
        self.a = np.array([a for a, _ in self.spans])
        self.L = np.array([b - a for a, b in self.spans])
        self.index_tapes = {k: tape(con, k, "index") for k in INDICES}
        self.hyg = tapes.get("HYG")
        self._market()
        self._calendar()
        self._second_pass()
        self._excess()
        log(f"panel built ({time.time() - t0:.0f}s)")

    # -------------------------------------------------------------- storage

    def put(self, name, si, idx, values):
        if name not in self.D:
            self.D[name] = np.full((self.m, self.n), np.nan)
        self.D[name][si, idx] = values

    def put_i(self, name, si, idx, values, dtype=np.float32):
        if name not in self.I:
            self.I[name] = np.full((self.m, self.n, S15), np.nan, dtype=dtype)
        self.I[name][si, idx] = values

    # -------------------------------------------------------------- coverage

    def _coverage(self, registered, tapes, ok):
        rows = []
        for s in registered:
            days, g = tapes[s]
            good = days[ok[s]]
            row = dict(symbol=s, etf=is_etf(s), sessions=int(ok[s].sum()), partial_dropped=int((~ok[s]).sum()))
            if not len(days):
                row.update(role="no bars")
            else:
                first, last = good[0] if len(good) else days[0], good[-1] if len(good) else days[-1]
                lo, hi = bisect.bisect_left(self.sess, first), bisect.bisect_right(self.sess, last)
                row.update(first=str(first), last=str(last),
                           missing=int(sum(1 for d in self.sess[lo:hi] if d not in set(good))),
                           lag=len(self.sess) - hi,
                           discovery=int(sum(d < SPLIT for d in good)), holdout=int(sum(d >= SPLIT for d in good)),
                           role="context" if s in CONTEXT_ONLY else "universe" if s in self.syms else "too short")
            rows.append(row)
        return rows

    # -------------------------------------------------------------- per symbol

    def _symbol(self, si, idx, g):
        O, H, L, C, V = fold(g).T
        r = returns5(g)
        RV = np.nansum(r * r, 1)
        BV = math.pi / 2 * np.nansum(np.abs(r[:, 1:]) * np.abs(r[:, :-1]), 1)
        ret, ov = np.log(C / shift(C)), np.log(O / shift(C))
        sig = ti.rstd(ret, 20, ddof=1)
        for k, v in dict(open=O, high=H, low=L, close=C, volume=V, rv=RV, bv=BV, ret=ret, ov=ov, sigma=sig,
                         sigma_prev=shift(sig), avgp=np.nanmean(g[:, :, 3], 1)).items():
            self.put(k, si, idx, v)

        s, aux = ti.daily(dict(open=O, high=H, low=L, close=C, volume=V, rv=RV, bv=BV, ov=ov))
        for k, v in s.items():
            self.put(k, si, idx, v)
        self.put("adx", si, idx, aux["adx"])
        trend = np.array([aux[k] for k in ti.TREND_CONSENSUS])
        self.put("trend_count", si, idx, np.where(np.isnan(trend).any(0), np.nan, trend.sum(0)))
        bull, bear = np.array(aux["osc_bull"]), np.array(aux["osc_bear"])
        net = bull.sum(0) - bear.sum(0)
        self.put("osc_net", si, idx, np.where(np.isnan(bull).any(0) | np.isnan(bear).any(0), np.nan, net))

        # Targets: enter at the next open, sd units of the horizon.
        rvt = RV + ov * ov
        for h in HORIZONS:
            R = np.log(lead(C, h) / lead(O, 1))
            rvp = sum(lead(RV, k) for k in range(1, h + 1)) + sum(lead(ov, k) ** 2 for k in range(2, h + 1))
            self.put(f"R{h}", si, idx, R)
            self.put(f"X{h}", si, idx, R / (sig * math.sqrt(h)))
            self.put(f"RVP{h}", si, idx, rvp)
            self.put(f"M{h}", si, idx, 0.5 * np.log(rvp / (h * sig * sig)))
        for k in range(1, PATH + 1):
            self.put(f"P{k}", si, idx, np.log(lead(C, k) / lead(O, 1)) / sig)
        self.put("lnd", si, idx, np.log(rvt))
        self.put("lnw", si, idx, np.log(ti.sma(rvt, 5)))
        self.put("lnm", si, idx, np.log(ti.sma(rvt, 22)))

        # Trailing daily vol, seven ways (section D).
        hl, co = np.log(H / L), np.log(C / O)
        rs = np.log(H / C) * np.log(H / O) + np.log(L / C) * np.log(L / O)
        k = 0.34 / (1.34 + 21 / 19)
        est = dict(cc20=sig,
                   parkinson20=np.sqrt(ti.sma(hl * hl, 20) / (4 * math.log(2))),
                   garman_klass20=np.sqrt(ti.sma(0.5 * hl * hl - (2 * math.log(2) - 1) * co * co, 20)),
                   rogers_satchell20=np.sqrt(ti.sma(rs, 20)),
                   yang_zhang20=np.sqrt(ti.rstd(ov, 20, 1) ** 2 + k * ti.rstd(co, 20, 1) ** 2 + (1 - k) * ti.sma(rs, 20)),
                   rv20=np.sqrt(ti.sma(rvt, 20)),
                   ewma94=np.sqrt(ti.ema(ret * ret, 20, 0.06)))
        for name, v in est.items():
            self.put(f"est_{name}", si, idx, v)

        for name, v in barrier(O, C, g[:, :, 1], g[:, :, 2], sig).items():
            self.put(name, si, idx, v)
        self._intraday(si, idx, g, sig)
        self._levels(idx, g, O, H, L, C, sig)

    def _levels(self, idx, g, O, H, L, C, sig):
        """Each session's support/resistance levels, all known before they can
        be tested, and the 5-minute inputs a touch is scored on (section H)."""
        n = len(g)
        lv = {k: shift(v) for k, v in {**ti.pivots(H, L, C), **ti.fibs(H, L)}.items()}
        lv.update(prev_high=shift(H), prev_low=shift(L), prev_close=shift(C))
        va = np.array([ti.value_area(g[p, :, 1], g[p, :, 2], g[p, :, 4]) for p in range(n)])
        lv["poc"], lv["vah"], lv["val"] = (shift(va[:, k]) for k in range(3))
        lv["ib_high"], lv["ib_low"] = np.nanmax(g[:, :12, 1], 1), np.nanmin(g[:, :12, 2], 1)
        flat = g[:, :, 3].ravel()
        okb = ~np.isnan(flat)
        r = np.full(flat.shape, np.nan)
        r[okb] = ti.rsi(flat[okb], 14)
        vol = g[:, :, 4]
        sma50 = shift(ti.sma(C, 50))
        self.lv.append(dict(H=g[:, :, 1], Lo=g[:, :, 2], C=g[:, :, 3], O0=O, sigp=shift(sig), ref_ib=g[:, 11, 3],
                            levels={x["name"]: lv[x["name"]] for x in ti.LEVELS}, rsi=r.reshape(n, S5),
                            rvol=vol / pd.DataFrame(vol).rolling(20, min_periods=20).median().shift(1).to_numpy(),
                            up=ti.flag(shift(C) > sma50, sma50), idx=idx))

    def _intraday(self, si, idx, g, sig):
        n = len(g)
        b = fold(g.reshape(n, S15, 3, 5))  # (n, 26, 5)
        o, h, l, c, v = (b[..., k] for k in range(5))
        flat = b.reshape(-1, 5)
        okb = ~np.isnan(flat[:, 3])
        s = ti.intraday({k: flat[okb, i] for i, k in enumerate(("open", "high", "low", "close", "volume"))})
        for k, x in s.items():
            full = np.full(n * S15, np.nan)
            full[okb] = x
            self.put_i(k, si, idx, full.reshape(n, S15))
        sp = shift(sig)[:, None]  # yesterday's sd: known before the session
        tp = (h + l + c) / 3
        vwap = np.nancumsum(tp * v, 1) / np.nancumsum(v, 1)
        self.put_i("i_vwap_dist", si, idx, np.where(np.isnan(c), np.nan, (c - vwap) / vwap / sp))
        med = pd.DataFrame(v).rolling(20, min_periods=20).median().shift(1).to_numpy()
        rvol = v / med
        self.put_i("i_rvol", si, idx, rvol)
        self.put_i("i_rvol_up", si, idx, ti.flag((rvol > 2) & (c > o), rvol))
        self.put_i("i_rvol_dn", si, idx, ti.flag((rvol > 2) & (c < o), rvol))
        ib = np.log(np.nanmax(h[:, :4], 1) / np.nanmin(l[:, :4], 1)) / sp[:, 0]
        ratio = ib / pd.Series(ib).rolling(20, min_periods=20).median().shift(1).to_numpy()
        ibr = np.full((n, S15), np.nan)
        ibr[:, 3:] = ratio[:, None]  # the first hour is known at 10:30
        self.put_i("i_ib_ratio", si, idx, ibr)
        last = last_valid(c)
        gap = np.full((n, S15), np.nan)
        gap[:, 0] = np.log(o[:, 0] / shift(last)) / sp[:, 0]
        self.put_i("i_gap", si, idx, gap)
        self.put_i("i_abs_gap", si, idx, np.abs(gap))

        ent = np.full((n, S15), np.nan)
        ent[:, :-1] = o[:, 1:]
        self.put_i("CLOSE", si, idx, np.log(last[:, None] / ent) / sp, np.float64)
        self.put_i("NEXT", si, idx, np.log(lead(last, 1)[:, None] / ent) / sp, np.float64)
        r = returns5(g)
        rc = np.cumsum(np.nan_to_num(r * r)[:, ::-1], 1)[:, ::-1]
        rest = np.full((n, S15), np.nan)
        j = np.arange(S15 - 1)
        rest[:, :-1] = rc[:, 3 * (j + 1)]
        frac = np.r_[(S5 - 3 * (j + 1)) / S5, np.nan]
        with np.errstate(divide="ignore"):
            m = 0.5 * np.log(rest / (sp * sp * frac))
        m[np.isnan(ent) | (rest <= 0)] = np.nan
        self.put_i("MREST", si, idx, m, np.float64)

    # -------------------------------------------------------------- shared series

    def daily_close(self, days, g):
        out = np.full(self.n, np.nan)
        if len(days):
            c = last_valid(g[:, :, 3])
            for d, x in zip(days, c):
                i = self.pos.get(d)
                if i is not None:
                    out[i] = x
        return out

    def _market(self):
        self.mk = {k: self.daily_close(*self.index_tapes[k]) for k in INDICES}
        self.mk["HYG"] = self.daily_close(*self.hyg) if self.hyg else np.full(self.n, np.nan)
        self.spy = self.syms.index("SPY")
        self.mk["SPY"] = self.D["close"][self.spy]
        mk = ti.market(self.mk["SPY"], self.mk["VIX"], self.mk["VIX3M"], self.mk["VIX9D"], self.mk["VVIX"],
                       self.mk["HYG"], self.D["close"])
        valid = ~np.isnan(self.D["close"])
        for k, v in mk.items():
            self.D[k] = np.where(valid, np.broadcast_to(v, (self.m, self.n)), np.nan)

    def _calendar(self):
        n, valid = self.n, ~np.isnan(self.D["close"])
        wd = np.array([d.weekday() for d in self.sess])
        nxt = np.r_[wd[1:], -1]
        for k, name in enumerate(("mon", "tue", "wed", "thu", "fri")):
            v = np.where(nxt == -1, np.nan, (nxt == k).astype(float))
            self.D[f"next_{name}"] = np.where(valid, v, np.nan)
        month = np.array([d.month for d in self.sess])
        last = np.r_[month[1:] != month[:-1], True]           # last session of a month
        rank = np.zeros(n, int)
        for i in range(1, n):
            rank[i] = 0 if month[i] != month[i - 1] else rank[i - 1] + 1
        tom = last | (rank < 3)
        self.D["turn_of_month"] = np.where(valid, np.r_[tom[1:], False].astype(float), np.nan)
        self.D["gap"] = self.D["ov"] / self.D["sigma_prev"]

        rel = release_days(self.con)
        soon = np.zeros((self.m, n))
        up, dn = np.zeros((self.m, n)), np.zeros((self.m, n))
        self.release_count = {}
        for si, s in enumerate(self.syms):
            react = np.zeros(n)
            for d in rel.get(s, ()):
                i = bisect.bisect_left(self.sess, d)
                if i < n and self.sess[i] - d < dt.timedelta(days=4):
                    react[i] = 1
            self.release_count[s] = int(react.sum())
            cs = np.cumsum(react)
            ahead = cs[np.minimum(np.arange(n) + BARRIER, n - 1)] - cs
            soon[si] = ahead > 0
            move = self.D["ret"][si] / self.D["sigma_prev"][si]
            up[si], dn[si] = (react == 1) & (move > 1), (react == 1) & (move < -1)
        self.D["earnings_soon"] = np.where(valid, soon, np.nan)
        self.D["earnings_up"] = np.where(valid & ~np.isnan(self.D["sigma_prev"]), up, np.nan)
        self.D["earnings_dn"] = np.where(valid & ~np.isnan(self.D["sigma_prev"]), dn, np.nan)
        macro = {dt.date.fromisoformat(r[0]) for r in self.con.execute(
            "SELECT DISTINCT event_date FROM events WHERE kind IN ('cpi', 'jobs_report')")}
        nxt_macro = np.array([self.sess[i + 1] in macro if i + 1 < n else False for i in range(n)], float)
        self.D["macro_next"] = np.where(valid, nxt_macro, np.nan)
        self.macro_days = len([d for d in macro if self.sess[0] <= d <= self.sess[-1]])

        # Contexts (section F): states every signal is crossed with.
        vix = self.mk["VIX"]
        q1, q2 = (pd.Series(vix).rolling(252, min_periods=252).quantile(q).to_numpy() for q in (1 / 3, 2 / 3))
        ctx = dict(sym_above_200=self.D["above_sma200"], spy_above_200=self.D["spy_above_200"],
                   adx_trending=ti.flag(self.D["adx"] > 25, self.D["adx"]),
                   adx_flat=ti.flag(self.D["adx"] < 20, self.D["adx"]),
                   vix_backwardation=self.D["vix_backwardation"],
                   vix_top_third=np.broadcast_to(ti.flag(vix > q2, q2), (self.m, n)),
                   vix_bottom_third=np.broadcast_to(ti.flag(vix < q1, q1), (self.m, n)),
                   earnings_near=self.D["earnings_soon"])
        rvq, prev = np.full((self.m, n, 2), np.nan), np.full((self.m, n), np.nan)
        for si, idx in enumerate(self.idx):
            sg = pd.Series(self.D["sigma"][si, idx])
            rvq[si, idx, 0] = sg.rolling(252, min_periods=252).quantile(1 / 3).to_numpy()
            rvq[si, idx, 1] = sg.rolling(252, min_periods=252).quantile(2 / 3).to_numpy()
            a = self.D["avgp"][si, idx]
            prev[si, idx] = a - shift(a, 5)
        ctx["rv_top_third"] = ti.flag(self.D["sigma"] > rvq[..., 1], rvq[..., 1])
        ctx["rv_bottom_third"] = ti.flag(self.D["sigma"] < rvq[..., 0], rvq[..., 0])
        ctx["prevailing_up"], ctx["prevailing_down"] = ti.flag(prev > 0, prev), ti.flag(prev < 0, prev)
        self.ctx = {k: np.where(valid, ctx[k], np.nan) for k in CONTEXTS}

    def _second_pass(self):
        """What needs SPY or every symbol first: the hedged target, relative
        strength, and the trailing quintiles of each continuous signal."""
        spy_ret, spy_close, spy_r5 = self.D["ret"][self.spy], self.D["close"][self.spy], self.D["R5"][self.spy]
        self.D["XH5"] = np.full((self.m, self.n), np.nan)
        self.D["rs63"] = np.full((self.m, self.n), np.nan)
        for si, idx in enumerate(self.idx):
            if si == self.spy:
                continue
            c, sc = self.D["close"][si, idx], spy_close[idx]
            sh = ti.rstd(self.D["ret"][si, idx] - spy_ret[idx], 20, 1)
            self.D["XH5"][si, idx] = (self.D["R5"][si, idx] - spy_r5[idx]) / (sh * math.sqrt(5))
            self.D["rs63"][si, idx] = c / shift(c, 63) - sc / shift(sc, 63)
        self.q: dict[str, np.ndarray] = {}
        for name, s in ti.SIGNALS.items():
            if s["kind"] != "continuous":
                continue
            src = self.I if s["timeframe"] == "I" else self.D
            code = np.full(src[name].shape, np.nan, dtype=np.float32)
            for si, idx in enumerate(self.idx):
                x = src[name][si, idx].astype(float)
                if s["timeframe"] == "I":
                    flat = x.ravel()
                    per = (~np.isnan(flat)).sum() / max(1, (~np.isnan(x)).any(1).sum())
                    code[si, idx] = quintile_codes(flat, per).reshape(x.shape)
                else:
                    code[si, idx] = quintile_codes(x, 1.0)
            self.q[name] = code

    def _excess(self):
        """Each target less its symbol's mean in the same period (and, for
        15-minute bars, the same slot), so drift and intraday seasonality are
        not credited to a signal."""
        self.EX = {}
        targets = {**{f"X{h}": self.D[f"X{h}"] for h in HORIZONS}, "XH5": self.D["XH5"],
                   **{f"M{h}": self.D[f"M{h}"] for h in (1, 5)},
                   **{f"P{k}": self.D[f"P{k}"] for k in range(1, PATH + 1)},
                   "RB": self.D["RB"], "RS": self.D["RS"],
                   "CLOSE": self.I["CLOSE"], "NEXT": self.I["NEXT"], "MREST": self.I["MREST"]}
        for k, x in targets.items():
            out = x.astype(float).copy()
            for p in (self.disc, ~self.disc):
                out[:, p] -= np.nanmean(x[:, p], axis=1, keepdims=True)
            self.EX[k] = out


def barrier(O, C, H5, L5, sig):
    """Triple barrier per decision session: enter at the next open, +/-1
    trailing weekly sd, 5 sessions, first touch on the 5-minute path. A bar
    that reaches both is the stop, whichever way the trade faces."""
    n, W = len(O), BARRIER
    keys = ("RB", "RS", "UPF", "DNF", "TIE", "TOUT", "MFE", "MAE", "BARS")
    out = {k: np.full(n, np.nan) for k in keys}
    if n < W + 2:
        return out
    Hw = sliding_window_view(H5, W, axis=0).transpose(0, 2, 1).reshape(n - W + 1, W * S5)
    Lw = sliding_window_view(L5, W, axis=0).transpose(0, 2, 1).reshape(n - W + 1, W * S5)
    p = np.arange(n - W)
    entry, b = O[p + 1], sig[p] * math.sqrt(W)
    hi, lo = np.log(Hw[p + 1] / entry[:, None]), np.log(Lw[p + 1] / entry[:, None])
    never = W * S5
    uh, dl = hi >= b[:, None], lo <= -b[:, None]
    iu = np.where(uh.any(1), uh.argmax(1), never)
    idn = np.where(dl.any(1), dl.argmax(1), never)
    r = np.log(C[p + W] / entry) / b
    upf, dnf = iu < idn, idn < iu
    tie, tout = (iu == idn) & (iu < never), (iu == never) & (idn == never)
    vals = dict(RB=np.where(upf, 1.0, np.where(dnf | tie, -1.0, np.clip(r, -1, 1))),
                RS=np.where(dnf, 1.0, np.where(upf | tie, -1.0, np.clip(-r, -1, 1))),
                UPF=upf, DNF=dnf, TIE=tie, TOUT=tout,
                MFE=np.nanmax(hi, 1) / b, MAE=np.nanmin(lo, 1) / b, BARS=np.minimum(iu, idn))
    good = np.isfinite(b) & np.isfinite(entry) & np.isfinite(r)
    for k in keys:
        out[k][p] = np.where(good, np.asarray(vals[k], float), np.nan)
    return out


# ------------------------------------------------------------------ statistics

@lru_cache(maxsize=None)
def boot_idx(g: int) -> np.ndarray:
    return np.random.default_rng(1).integers(0, g, (BOOT, g))


def ratio_t(s, c, null=0.0, rows=None):
    k = c > 0
    s, c = s[k], c[k]
    g = len(c)
    if (c.sum() if rows is None else rows[k].sum()) < MIN_N or g < MIN_WEEKS:
        return None
    p = s.sum() / c.sum()
    e = s - p * c
    se = math.sqrt(g / (g - 1) * (e * e).sum()) / c.sum()
    return (p - null) / se if se > 1e-12 else None


def ratio_cell(s, c, unit, null=0.0, rows=None) -> dict:
    """`news_study.stat` on pre-aggregated blocks: the ratio of sums, its
    block-bootstrap 95 % interval and a block-clustered t. `rows` counts the
    observations per block when the denominator is not a count (CRPS sums)."""
    k = c > 0
    s, c = s[k], c[k]
    rows = c if rows is None else rows[k]
    n, g = int(rows.sum()), len(c)
    out = dict(n=n, weeks=g, block=unit, null=null, point=r5(s.sum() / c.sum()) if n else None, lo=None, hi=None, t=None)
    if n < MIN_N or g < MIN_WEEKS:
        return out
    idx = boot_idx(g)
    b = np.sort(s[idx].sum(1) / c[idx].sum(1))
    out.update(lo=r5(b[int(0.025 * BOOT)]), hi=r5(b[int(0.975 * BOOT) - 1]), t=r5(ratio_t(s, c, null, rows)))
    return out


def diff_parts(si, ci, so, co):
    k = (ci + co) > 0
    return si[k], ci[k], so[k], co[k]


def diff_t(si, ci, so, co, null=0.0):
    si, ci, so, co = diff_parts(si, ci, so, co)
    g, ni, no = len(ci), ci.sum(), co.sum()
    if ni < MIN_N or no < MIN_N or g < MIN_WEEKS:
        return None
    ri, ro = si.sum() / ni, so.sum() / no
    e = (si - ri * ci) / ni - (so - ro * co) / no
    se = math.sqrt(g / (g - 1) * (e * e).sum())
    return (ri - ro - null) / se if se > 1e-12 else None


def diff_cell(si, ci, so, co, unit, null=0.0) -> dict:
    """In minus out, both ratios of sums, resampling blocks jointly."""
    si, ci, so, co = diff_parts(si, ci, so, co)
    ni, no, g = int(ci.sum()), int(co.sum()), len(ci)
    p = si.sum() / ni - so.sum() / no if ni and no else None
    out = dict(n=ni + no, n_in=ni, n_out=no, weeks=g, block=unit, null=null, point=r5(p), lo=None, hi=None, t=None)
    if ni < MIN_N or no < MIN_N or g < MIN_WEEKS:
        return out
    idx = boot_idx(g)
    a, b = ci[idx].sum(1), co[idx].sum(1)
    ok = (a > 0) & (b > 0)
    d = np.sort(si[idx].sum(1)[ok] / a[ok] - so[idx].sum(1)[ok] / b[ok])
    out.update(lo=r5(d[int(0.025 * len(d))]), hi=r5(d[int(0.975 * len(d)) - 1]), t=r5(diff_t(si, ci, so, co, null)))
    return out


def sums(rows, X, d, blk, m, hits=False):
    """Per (symbol, block) sums and counts of d * X over the given rows."""
    v = X[rows] * d
    ok = ~np.isnan(v)
    key = rows[0][ok] * blk.nb + blk.id[rows[1][ok]]
    size = m * blk.nb
    S = np.bincount(key, v[ok], size).reshape(m, blk.nb)
    C = np.bincount(key, minlength=size).reshape(m, blk.nb).astype(float)
    if hits:
        return S, C, np.bincount(key, (v[ok] > 0).astype(float), size).reshape(m, blk.nb)
    return S, C


def pick(S, gm, bm):
    return S[gm][:, bm].sum(0)


def rows_of(mask) -> tuple:
    return tuple(np.nonzero(mask))


def offsets(data: Data, draw: int) -> np.ndarray:
    rng = np.random.default_rng(10_000 + draw)
    k = np.full(data.m, -1)
    for si, L in enumerate(data.L):
        if L >= 2 * MIN_SHIFT:
            k[si] = rng.integers(MIN_SHIFT, L - MIN_SHIFT + 1)
    return k


def moved(rows, k, data: Data, back=False) -> tuple:
    """Rows of a signal circularly shifted by k[symbol] sessions within each
    symbol's span (back=True maps a row to where a shifted series reads from)."""
    sym = rows[0]
    ks = k[sym]
    keep = ks >= 0
    step = -ks if back else ks
    sess = data.a[sym] + (rows[1] - data.a[sym] + step) % data.L[sym]
    return (sym[keep], sess[keep]) + tuple(r[keep] for r in rows[2:])


def agreement(S, C, gm, bm, years) -> dict:
    tot = pick(S, gm, bm).sum() / max(pick(C, gm, bm).sum(), 1)
    sym = [S[i][bm].sum() / C[i][bm].sum() for i in np.flatnonzero(gm) if C[i][bm].sum() >= 20]
    yrs = []
    for y in sorted(set(years[bm])):
        b = bm & (years == y)
        if pick(C, gm, b).sum() >= 20:
            yrs.append(pick(S, gm, b).sum() / pick(C, gm, b).sum())
    same = lambda xs: f"{sum(np.sign(x) == np.sign(tot) for x in xs)}/{len(xs)}"
    return dict(symbols_agree=same(sym), years_agree=same(yrs))


def placebo(maxes: list[float], pairs: list[tuple], draws: int) -> dict:
    """The family's bar, the median shuffle's best |t|, and the real best."""
    real = [abs(p[0]["t"]) for p in pairs if p[0].get("t") is not None]
    return dict(t95=r5(placebo_cut(maxes)), t50=r5(float(np.median(maxes))) if maxes else None,
                best=r5(max(real, default=0.0)), draws=draws, cells=len(pairs))


def finish(pairs: list[tuple], cut: float) -> None:
    """Judge discovery cells against the family's cut; a finding's holdout
    twin becomes its single pre-registered test, the rest stay unguarded."""
    judge([p[0] for p in pairs], cut)
    for disc, hold, *_ in pairs:
        judge([hold], 0.0 if disc["verdict"] == "finding" else None)


# ------------------------------------------------------------------ searched families

def spec(data: Data, name: str, target: str, horizon, groups=GROUPS, X=None, blk=None, d=None) -> dict:
    s = ti.SIGNALS[name]
    src_sig = data.I if s["timeframe"] == "I" else data.D
    if s["kind"] == "continuous":
        rin, rout = rows_of(data.q[name] == 1), rows_of(data.q[name] == -1)
    else:
        rin, rout = rows_of(src_sig[name] == 1), None
    return dict(signal=name, family=s["family"], era=s["era"], timeframe=s["timeframe"], kind=s["kind"],
                target=target, horizon=horizon, rin=rin, rout=rout, X=X, d=s["sign"] if d is None else d,
                blk=blk, groups=groups)


def family(data: Data, specs: list[dict], draws: int, name: str) -> dict:
    """Cells for every spec x group x period, a circular-shift placebo over the
    discovery cells, verdicts."""
    t0 = time.time()
    cells, pairs = [], []
    labels = ("signal", "family", "era", "timeframe", "kind", "target", "horizon")
    for sp in specs:
        blk, X, d = sp["blk"], sp["X"], sp["d"]
        Si, Ci, Hi = sums(sp["rin"], X, d, blk, data.m, hits=True)
        So, Co = sums(sp["rout"], X, d, blk, data.m) if sp["rout"] else (None, None)
        for gname in sp["groups"]:
            gm = data.groups[gname]
            twin = []
            for period, bm in (("discovery", blk.disc), ("holdout", ~blk.disc)):
                if So is None:
                    c = ratio_cell(pick(Si, gm, bm), pick(Ci, gm, bm), blk.unit)
                    c["hit"] = r5(pick(Hi, gm, bm).sum() / max(pick(Ci, gm, bm).sum(), 1))
                    c.update(agreement(Si, Ci, gm, bm, blk.year))
                else:
                    c = diff_cell(pick(Si, gm, bm), pick(Ci, gm, bm), pick(So, gm, bm), pick(Co, gm, bm), blk.unit)
                c.update({k: sp[k] for k in labels}, group=gname, period=period)
                cells.append(c)
                twin.append(c)
            pairs.append((twin[0], twin[1], sp, gname))

    maxes = []
    for draw in range(draws):
        k = offsets(data, draw)
        best = 0.0
        for sp in specs:
            blk = sp["blk"]
            ri = moved(sp["rin"], k, data)
            ri = tuple(r[blk.disc[blk.id[ri[1]]]] for r in ri)
            Si, Ci = sums(ri, sp["X"], sp["d"], blk, data.m)
            if sp["rout"]:
                ro = moved(sp["rout"], k, data)
                ro = tuple(r[blk.disc[blk.id[ro[1]]]] for r in ro)
                So, Co = sums(ro, sp["X"], sp["d"], blk, data.m)
            for gname in sp["groups"]:
                gm = data.groups[gname]
                t = (ratio_t(pick(Si, gm, blk.disc), pick(Ci, gm, blk.disc)) if not sp["rout"] else
                     diff_t(pick(Si, gm, blk.disc), pick(Ci, gm, blk.disc), pick(So, gm, blk.disc), pick(Co, gm, blk.disc)))
                if t is not None:
                    best = max(best, abs(t))
        maxes.append(best)
    cut = placebo_cut(maxes)
    finish(pairs, cut)
    data.pairs[name] = pairs
    n_find = sum(p[0]["verdict"] == "finding" for p in pairs)
    log(f"{name}: {len(pairs)} discovery cells, cut |t| {cut:.2f}, {n_find} findings ({time.time() - t0:.0f}s)")
    return dict(placebo=placebo(maxes, pairs, draws), cells=cells)


def direction(data: Data, draws: int) -> dict:
    daily = [n for n, s in ti.SIGNALS.items() if s["timeframe"] == "D" and s["target"] == "direction"]
    targets = [("raw", 1, "X1", data.week), ("raw", 5, "X5", data.month), ("raw", 10, "X10", data.month),
               ("hedged", 5, "XH5", data.month)]
    specs = [spec(data, n, t, h, X=data.EX[x], blk=b) for n in daily for t, h, x, b in targets]
    out = dict(daily=family(data, specs, draws, "direction_daily"))
    intra = [n for n, s in ti.SIGNALS.items() if s["timeframe"] == "I" and s["target"] == "direction"]
    specs = [spec(data, n, "raw", h, X=data.EX[x], blk=data.week) for n in intra for h, x in (("close", "CLOSE"), ("next", "NEXT"))]
    out["intraday"] = family(data, specs, draws, "direction_intraday")
    return out


def move_size(data: Data, draws: int) -> dict:
    daily = [n for n, s in ti.SIGNALS.items() if s["timeframe"] == "D" and s["target"] == "move"]
    specs = [spec(data, n, "move", h, X=data.EX[x], blk=b) for n in daily for h, x, b in ((1, "M1", data.week), (5, "M5", data.month))]
    out = dict(daily=family(data, specs, draws, "move_daily"))
    intra = [n for n, s in ti.SIGNALS.items() if s["timeframe"] == "I" and s["target"] == "move"]
    specs = [spec(data, n, "move", "rest", X=data.EX["MREST"], blk=data.week) for n in intra]
    out["intraday"] = family(data, specs, draws, "move_intraday")
    out["har"] = har(data)
    out["estimators"] = estimators(data)
    out["vrp"] = vrp(data)
    return out


def entry_exit(data: Data, draws: int) -> dict:
    events = [n for n, s in ti.SIGNALS.items() if s["timeframe"] == "D" and s["target"] == "direction" and s["kind"] == "event"]
    specs = [spec(data, n, "barrier", BARRIER, X=data.EX["RB" if ti.SIGNALS[n]["sign"] > 0 else "RS"], blk=data.month, d=1)
             for n in events]
    out = dict(barrier=family(data, specs, draws, "barrier"))
    # The outcome mix behind each barrier cell, and the path after the fire.
    desc, paths = [], []
    gm = data.groups["all"]
    for n in events:
        d = ti.SIGNALS[n]["sign"]
        act = (data.D[n] == 1) & gm[:, None]
        for period, pm in (("discovery", data.disc), ("holdout", ~data.disc)):
            a = act & pm[None, :] & ~np.isnan(data.D["RB"])
            tgt, stp = (data.D["UPF"], data.D["DNF"]) if d > 0 else (data.D["DNF"], data.D["UPF"])
            mfe, mae = (data.D["MFE"], data.D["MAE"]) if d > 0 else (-data.D["MAE"], -data.D["MFE"])
            if a.sum() == 0:
                continue
            desc.append(dict(signal=n, period=period, n=int(a.sum()), p_target=r5(tgt[a].mean()),
                             p_stop=r5((stp[a] + data.D["TIE"][a]).mean()), p_timeout=r5(data.D["TOUT"][a].mean()),
                             mfe_median=r5(np.median(mfe[a])), mae_median=r5(np.median(mae[a])),
                             bars_median=r5(np.median(data.D["BARS"][a][data.D["TOUT"][a] == 0])) if (data.D["TOUT"][a] == 0).any() else None))
            p = act & pm[None, :]
            paths.append(dict(signal=n, period=period, mean=[r5(np.nanmean(d * data.EX[f"P{k}"][p])) for k in range(1, PATH + 1)]))
    base = []
    for period, pm in (("discovery", data.disc), ("holdout", ~data.disc)):
        a = ~np.isnan(data.D["RB"]) & pm[None, :]
        base.append(dict(period=period, n=int(a.sum()), p_up_first=r5(data.D["UPF"][a].mean()),
                         p_down_first=r5(data.D["DNF"][a].mean()), p_tie=r5(data.D["TIE"][a].mean()),
                         p_timeout=r5(data.D["TOUT"][a].mean())))
    out.update(outcomes=desc, paths=paths, baseline=base)
    return out


# ------------------------------------------------------------------ section D extras

def har(data: Data) -> dict:
    """Log HAR-RV (Corsi 2009), pooled over the universe, fit on discovery:
    the variance of the target's own path (open t+1 to close t+h)."""
    data.har = {}
    out = {}
    for h in HORIZONS:
        y = np.log(data.D[f"RVP{h}"] / h)
        X = np.stack([np.ones_like(y), data.D["lnd"], data.D["lnw"], data.D["lnm"]], -1)
        ok = np.isfinite(y) & np.isfinite(X).all(-1)
        fit = ok & data.disc[None, :]
        beta, *_ = np.linalg.lstsq(X[fit], y[fit], rcond=None)
        res = y[fit] - X[fit] @ beta
        s2 = res.var()
        data.har[h] = np.where(np.isfinite(X).all(-1), np.sqrt(h * np.exp(X @ beta + s2 / 2)), np.nan)
        out[str(h)] = dict(beta=[r5(b) for b in beta], resid_var=r5(s2), r2=r5(1 - s2 / y[fit].var()), n=int(fit.sum()))
    return out


SQPI, SQ2PI = math.sqrt(math.pi), math.sqrt(2 * math.pi)


def crps(mu, sig, y):
    """Gaussian CRPS, vectorized `news_study.crps_normal`."""
    z = (y - mu) / sig
    return sig * (z * ti.erf(z / math.sqrt(2)) + 2 * np.exp(-z * z / 2) / SQ2PI - 1 / SQPI)


def fit_scale(sig, y, grid=np.linspace(0.5, 1.8, 66)) -> float:
    return float(grid[np.argmin([crps(0, k * sig, y).mean() for k in grid])])


def skill_cell(c0, c1, sess, blk, period_mask) -> dict:
    """CRPS saved against the null, as a ratio of block sums."""
    m = period_mask[sess]
    key = blk.id[sess[m]]
    s = np.bincount(key, (c0 - c1)[m], blk.nb)
    c = np.bincount(key, c0[m], blk.nb)
    return ratio_cell(s, c, blk.unit, rows=np.bincount(key, minlength=blk.nb).astype(float))


def estimators(data: Data) -> list[dict]:
    """Which trailing vol forecasts the next 1 and 5 sessions best, each with
    one scale fit on discovery (range estimators miss the overnight gap)."""
    names = ["cc20", "parkinson20", "garman_klass20", "rogers_satchell20", "yang_zhang20", "rv20", "ewma94", "har"]
    out = []
    for h, blk in ((1, data.week), (5, data.month)):
        y = data.D[f"R{h}"]
        sig = {nm: (data.har[h] if nm == "har" else data.D[f"est_{nm}"] * math.sqrt(h)) for nm in names}
        ok = np.isfinite(y) & np.all([np.isfinite(s) & (s > 0) for s in sig.values()], 0)
        sess = np.nonzero(ok)[1]
        yv = y[ok]
        disc = data.disc[sess]
        scored = {}
        for nm in names:
            k = fit_scale(sig[nm][ok][disc], yv[disc])
            scored[nm] = (k, crps(0, k * sig[nm][ok], yv))
        null = scored["cc20"][1]
        for nm in names:
            for period, pm in (("discovery", data.disc), ("holdout", ~data.disc)):
                c = skill_cell(null, scored[nm][1], sess, blk, pm)
                judge([c], 0.0)
                signed([c])
                c.update(estimator=nm, horizon=h, period=period, scale=r5(scored[nm][0]),
                         mean_crps=r5(scored[nm][1][pm[sess]].mean()))
                out.append(c)
    return out


def vrp(data: Data) -> dict:
    """SPY's variance premium over the decade: VIX and VIX9D against the
    realized variance that followed, and whether a scaled VIX9D forecasts the
    next five sessions better than trailing vol. VIX prices SPX's whole
    strike strip, not the ATM IV the engine reads, so it sits above it."""
    s = data.spy
    ret, n = data.D["ret"][s], data.n
    vix, v9 = data.mk["VIX"], data.mk["VIX9D"]
    fwd = lambda k: (252 / k) * sum(lead(ret * ret, j) for j in range(1, k + 1))
    rv21, rv5 = fwd(21), fwd(5)
    years = np.array([d.year for d in data.sess])
    yearly = []
    for y in sorted(set(years)):
        m = years == y
        a, b = m & np.isfinite(vix) & np.isfinite(rv21), m & np.isfinite(v9) & np.isfinite(rv5)
        yearly.append(dict(year=int(y), n=int(a.sum()),
                           vix_ratio=r5(rv21[a].mean() / ((vix[a] / 100) ** 2).mean()) if a.any() else None,
                           vix_above=r5(((vix[a] / 100) ** 2 > rv21[a]).mean()) if a.any() else None,
                           vix9d_ratio=r5(rv5[b].mean() / ((v9[b] / 100) ** 2).mean()) if b.any() else None,
                           vix9d_above=r5(((v9[b] / 100) ** 2 > rv5[b]).mean()) if b.any() else None))
    y5 = data.D["R5"][s]
    root = math.sqrt(5 / 252)
    cands = dict(cc20=data.D["est_cc20"][s] * math.sqrt(5), har=data.har[5][s], vix=vix / 100 * root, vix9d=v9 / 100 * root)
    ok = np.isfinite(y5) & np.all([np.isfinite(c) for c in cands.values()], 0)
    sess = np.flatnonzero(ok)
    disc = data.disc[sess]
    fitted = {k: fit_scale(c[ok][disc], y5[ok][disc]) for k, c in cands.items()}
    scored = {k: (fitted[k], crps(0, fitted[k] * c[ok], y5[ok])) for k, c in cands.items()}
    scored["vix9d x0.85"] = (0.85, crps(0, 0.85 * cands["vix9d"][ok], y5[ok]))
    scored["vix9d x1.00"] = (1.0, crps(0, cands["vix9d"][ok], y5[ok]))
    cells = []
    for k, (scale, cr) in scored.items():
        for period, pm in (("discovery", data.disc), ("holdout", ~data.disc)):
            c = skill_cell(scored["cc20"][1], cr, sess, data.month, pm)
            judge([c], 0.0)
            signed([c])
            c.update(model=k, scale=r5(scale), period=period)
            cells.append(c)
    holdout_k = fit_scale(cands["vix9d"][ok][~disc], y5[ok][~disc])
    return dict(yearly=yearly, cells=cells, holdout_scale_vix9d=r5(holdout_k), first=str(data.sess[sess[0]]))


# ------------------------------------------------------------------ section F

def contexts(data: Data, draws: int) -> dict:
    """Every daily direction state/event inside each context, and the
    interaction (inside minus outside), five sessions out. The placebo shifts
    the context, not the signal: it asks whether the context's timing adds."""
    t0 = time.time()
    names = [n for n, s in ti.SIGNALS.items() if s["timeframe"] == "D" and s["target"] == "direction" and s["kind"] != "continuous"]
    X, blk, m = data.EX["X5"], data.month, data.m
    gm = data.groups["all"]
    ctx_in = {k: data.ctx[k] == 1 for k in CONTEXTS}
    ctx_out = {k: data.ctx[k] == 0 for k in CONTEXTS}
    prepared, cells, pairs = [], [], []
    for n in names:
        d = ti.SIGNALS[n]["sign"]
        rows = rows_of(data.D[n] == 1)
        v = X[rows] * d
        ok = ~np.isnan(v)
        rows = tuple(r[ok] for r in rows)
        prepared.append((n, d, rows, v[ok], rows[0] * blk.nb + blk.id[rows[1]]))
    size = m * blk.nb

    def split(rows, v, key, kin, kout):
        Si = np.bincount(key[kin], v[kin], size).reshape(m, blk.nb)
        Ci = np.bincount(key[kin], minlength=size).reshape(m, blk.nb).astype(float)
        So = np.bincount(key[kout], v[kout], size).reshape(m, blk.nb)
        Co = np.bincount(key[kout], minlength=size).reshape(m, blk.nb).astype(float)
        return Si, Ci, So, Co

    for n, d, rows, v, key in prepared:
        for c in CONTEXTS:
            Si, Ci, So, Co = split(rows, v, key, ctx_in[c][rows], ctx_out[c][rows])
            twin = []
            for period, bm in (("discovery", blk.disc), ("holdout", ~blk.disc)):
                a = [pick(x, gm, bm) for x in (Si, Ci, So, Co)]
                inside = ratio_cell(a[0], a[1], blk.unit)
                inside["verdict"] = "descriptive"
                inter = diff_cell(*a, blk.unit)
                inter.update(signal=n, context=c, period=period, family=ti.SIGNALS[n]["family"],
                             inside=inside, horizon=5, target="raw")
                cells.append(inter)
                twin.append(inter)
            pairs.append((twin[0], twin[1], dict(signal=n, context=c), "all"))
    maxes = []
    disc_rows = [(n, d, tuple(r[blk.disc[blk.id[rows[1]]]] for r in rows), v[blk.disc[blk.id[rows[1]]]],
                  key[blk.disc[blk.id[rows[1]]]]) for n, d, rows, v, key in prepared]
    for draw in range(draws):
        k = offsets(data, draw)
        best = 0.0
        for n, d, rows, v, key in disc_rows:
            src = moved(rows, k, data, back=True)
            keep = k[rows[0]] >= 0
            v2, key2 = v[keep], key[keep]
            for c in CONTEXTS:
                Si, Ci, So, Co = split(src, v2, key2, ctx_in[c][src], ctx_out[c][src])
                t = diff_t(*(pick(x, gm, blk.disc) for x in (Si, Ci, So, Co)))
                if t is not None:
                    best = max(best, abs(t))
        maxes.append(best)
    cut = placebo_cut(maxes)
    finish(pairs, cut)
    data.pairs["context"] = pairs
    log(f"context: {len(pairs)} discovery cells, cut |t| {cut:.2f}, "
        f"{sum(p[0]['verdict'] == 'finding' for p in pairs)} findings ({time.time() - t0:.0f}s)")
    return dict(placebo=placebo(maxes, pairs, draws), cells=cells)


def recent(mask):
    out = mask.copy()
    for j in range(1, RECENT + 1):
        out[:, j:] |= mask[:, :-j]
    return out


def roll_rows(mask, k, data: Data):
    out = np.zeros_like(mask)
    for si, (a, b) in enumerate(data.spans):
        if k[si] >= 0:
            out[si, a:b] = np.roll(mask[si, a:b], k[si])
    return out


def confirmation(data: Data, draws: int) -> dict:
    """Cross-family pairs agreeing within RECENT sessions (one firing today)
    against either firing alone, both directions pooled, five sessions out.
    The placebo shifts the second signal of the pair."""
    t0 = time.time()
    X, blk, m, gm = data.EX["X5"], data.month, data.m, data.groups["all"]
    core = [(fam, lab, data.D[up] == 1, data.D[dn] == 1) for fam, items in CORE.items() for lab, up, dn in items]
    combos = [(a, b) for i, a in enumerate(core) for b in core[i + 1:] if a[0] != b[0]]

    def masks(A, B):
        """(combo, alone) per direction."""
        out = []
        for d, (a, b) in ((1, (A[2], B[2])), (-1, (A[3], B[3]))):
            both = recent(a) & recent(b) & (a | b)
            out.append((d, both, (a | b) & ~both))
        return out

    def blocks(ms, bm=None):
        acc = [np.zeros((m, blk.nb)) for _ in range(4)]
        for d, both, alone in ms:
            for j, mask in ((0, both), (2, alone)):
                S, C = sums(rows_of(mask), X, d, blk, m)
                acc[j] += S
                acc[j + 1] += C
        return acc

    cells, pairs = [], []
    for A, B in combos:
        acc = blocks(masks(A, B))
        twin = []
        for period, bm in (("discovery", blk.disc), ("holdout", ~blk.disc)):
            a = [pick(x, gm, bm) for x in acc]
            combo = ratio_cell(a[0], a[1], blk.unit)
            combo["verdict"] = "descriptive"
            lift = diff_cell(*a, blk.unit)
            lift.update(a=A[1], b=B[1], families=f"{A[0]} x {B[0]}", period=period, combo=combo, horizon=5)
            cells.append(lift)
            twin.append(lift)
        pairs.append((twin[0], twin[1], dict(a=A[1], b=B[1]), "all"))
    maxes = []
    for draw in range(draws):
        k = offsets(data, draw)
        shifted = {lab: (roll_rows(up, k, data), roll_rows(dn, k, data)) for _f, lab, up, dn in core}
        best = 0.0
        for A, B in combos:
            Bs = (B[0], B[1]) + shifted[B[1]]
            acc = blocks(masks(A, Bs))
            t = diff_t(*(pick(x, gm, blk.disc) for x in acc))
            if t is not None:
                best = max(best, abs(t))
        maxes.append(best)
    cut = placebo_cut(maxes)
    finish(pairs, cut)
    data.pairs["pairs"] = pairs
    log(f"pairs: {len(pairs)} discovery cells, cut |t| {cut:.2f}, "
        f"{sum(p[0]['verdict'] == 'finding' for p in pairs)} findings ({time.time() - t0:.0f}s)")
    return dict(placebo=placebo(maxes, pairs, draws), cells=cells)


def consensus(data: Data) -> dict:
    """How many trend states agree, and the oscillators' net oversold count,
    in five bins each; the top-minus-bottom spread is one pre-registered test
    per index (more agreement, more return)."""
    X, blk, m, gm = data.EX["X5"], data.month, data.m, data.groups["all"]
    idx = dict(trend=(data.D["trend_count"], [(0, 2), (3, 4), (5, 5), (6, 7), (8, 10)]),
               oscillator=(data.D["osc_net"], [(-9, -3), (-2, -1), (0, 0), (1, 2), (3, 9)]))
    out = {}
    for name, (v, bins) in idx.items():
        rows, tests = [], []
        sums_ = []
        for lo, hi in bins:
            S, C = sums(rows_of((v >= lo) & (v <= hi)), X, 1, blk, m)
            sums_.append((S, C))
            for period, bm in (("discovery", blk.disc), ("holdout", ~blk.disc)):
                c = ratio_cell(pick(S, gm, bm), pick(C, gm, bm), blk.unit)
                c.update(index=name, bin=f"{lo}..{hi}" if lo != hi else str(lo), period=period, verdict="descriptive")
                rows.append(c)
        for period, bm in (("discovery", blk.disc), ("holdout", ~blk.disc)):
            (St, Ct), (Sb, Cb) = sums_[-1], sums_[0]
            c = diff_cell(pick(St, gm, bm), pick(Ct, gm, bm), pick(Sb, gm, bm), pick(Cb, gm, bm), blk.unit)
            judge([c], 0.0)
            c.update(index=name, period=period, test="top minus bottom bin")
            tests.append(c)
        out[name] = dict(bins=rows, tests=tests)
    return out


# ------------------------------------------------------------------ section H

def touches(z: dict, L: np.ndarray, start: int, others: np.ndarray):
    """First touch of a level per session: the first 5-minute bar from `start`
    whose range holds it. The side is where price came from (support from
    above, resistance from below). Outcome over the next WIN bars: +1 if price
    gets BAND daily sd away on its own side first, -1 if it gets that far
    through (a bar that reaches both counts as through), else the fraction of
    the way it ended. Confirmers are read at the touch bar's close."""
    H, Lo, C = z["H"], z["Lo"], z["C"]
    n = len(L)
    rows = np.arange(n)
    with np.errstate(invalid="ignore", divide="ignore"):
        hit = (Lo <= L[:, None]) & (H >= L[:, None]) & (np.arange(S5) >= start)
        t = hit.argmax(1)
        prev = np.where(t > start, C[rows, np.maximum(t - 1, 0)], z["O0"] if start == 0 else C[:, start - 1])
        side = np.sign(prev - L)
        w = t[:, None] + 1 + np.arange(WIN)
        inside = w < S5
        wc = np.minimum(w, S5 - 1)
        hw, lw, cw = (np.where(inside, np.take_along_axis(a, wc, 1), np.nan) for a in (H, Lo, C))
        b = (BAND * z["sigp"])[:, None]
        up, dn = np.log(hw / L[:, None]), np.log(lw / L[:, None])
        sup = side[:, None] > 0
        away, thru = np.where(sup, up >= b, dn <= -b), np.where(sup, dn <= -b, up >= b)
        ia = np.where(away.any(1), away.argmax(1), WIN)
        ib = np.where(thru.any(1), thru.argmax(1), WIN)
        last = last_valid(cw)
        disp = np.clip(side * np.log(last / L) / b[:, 0], -1, 1)
        R = np.where(ia < ib, 1.0, np.where(ib < WIN, -1.0, disp))
        cls = np.where(ia < ib, 0, np.where(ib < WIN, 1, 2))
        ok = hit.any(1) & (side != 0) & np.isfinite(b[:, 0]) & np.isfinite(last) & np.isfinite(L)
        rsi, close, rvol = z["rsi"][rows, t], C[rows, t], z["rvol"][rows, t]
        near = np.nanmin(np.abs(np.log(others / L)), 0) / z["sigp"] if len(others) else np.full(n, np.inf)
        conf = np.array([((side > 0) & (rsi < 30)) | ((side < 0) & (rsi > 70)),
                         ((side > 0) & (close > L)) | ((side < 0) & (close < L)),
                         rvol > 1.5,
                         ((side > 0) & (z["up"] == 1)) | ((side < 0) & (z["up"] == 0)),
                         near < CONFLUENCE])
    return ok, side, R, cls, conf


def level_pass(data: Data, k: np.ndarray | None):
    """Block sums of the touch outcome for every level type, side, confirmer
    and group: real levels when k is None, else every level moved to the
    distance (in sd) it had k sessions earlier from the price where testing
    starts — the open, or 10:30 for the initial balance. One shift for every
    type, so their geometry (and confluence) is kept."""
    T, Q, nb = len(ti.LEVELS), 1 + len(ti.CONFIRMERS), data.week.nb
    A = np.zeros((T, 2, Q, 2, 3, nb))      # type, side, confirmer, (sum, count), group, block
    K = np.zeros((T, 2, 5))                 # type, period, (reject, break, timeout, touched, eligible)
    for si, z in enumerate(data.lv):
        stack = np.array([z["levels"][x["name"]] for x in ti.LEVELS])
        if k is not None:
            if k[si] < 0:
                continue
            ref = np.array([z["ref_ib"] if x["start"] == "10:30" else z["O0"] for x in ti.LEVELS])
            d = np.log(stack / ref) / z["sigp"]
            stack = ref * np.exp(np.roll(d, k[si] % stack.shape[1], 1) * z["sigp"])
        blk = data.week.id[z["idx"]]
        per = (~data.disc[z["idx"]]).astype(int)
        gi = (0, 1 if data.etf[si] else 2)
        known = np.array([x["start"] == "open" for x in ti.LEVELS])
        for j, x in enumerate(ti.LEVELS):
            L = stack[j]
            # Confluence counts only levels already known: before 10:30 the
            # first hour's high and low are not, and a touch near the morning's
            # low would otherwise be "confirmed" by the low it is about to make.
            mates = (known if x["start"] == "open" else np.ones(len(known), bool)) & (np.arange(len(known)) != j)
            ok, side, R, cls, conf = touches(z, L, 0 if x["start"] == "open" else 12, stack[mates])
            for p in (0, 1):
                m = ok & (per == p)
                K[j, p, :3] += np.bincount(cls[m], minlength=3)
                K[j, p, 3] += m.sum()
                K[j, p, 4] += (np.isfinite(L) & np.isfinite(z["sigp"]) & (per == p)).sum()
            for s_i, sg in enumerate((1, -1)):
                base = ok & (side == sg)
                for q in range(Q):
                    m = base if q == 0 else base & conf[q - 1]
                    S, Cn = np.bincount(blk[m], R[m], nb), np.bincount(blk[m], minlength=nb)
                    for g in gi:
                        A[j, s_i, q, 0, g] += S
                        A[j, s_i, q, 1, g] += Cn
    return A, K


def levels(data: Data, draws: int) -> dict:
    """Section H. The statistic is the touch outcome at real levels minus the
    same at placebo levels (the mean of LEVEL_DRAWS shifted sets, in the same
    weeks); the search bar comes from scoring each placebo set against the
    others. A combination asks whether a confirmed real level beats a
    confirmed placebo level: a rejection bar flatters any level, so the
    confirmer alone is not the question."""
    t0 = time.time()
    n_draws = max(2, min(draws, LEVEL_DRAWS))
    Ar, Kr = level_pass(data, None)
    sims = [level_pass(data, offsets(data, 100_000 + i)) for i in range(n_draws)]
    Ap = np.array([a for a, _ in sims])
    Kp = np.mean([k for _, k in sims], 0)
    Ar = np.concatenate([Ar, Ar.sum(0, keepdims=True)])          # + "all levels"
    Ap = np.concatenate([Ap, Ap.sum(1, keepdims=True)], 1)
    base, tot = Ap.mean(0), Ap.sum(0)
    names = [x["name"] for x in ti.LEVELS] + ["all levels"]
    group_of = {x["name"]: x["group"] for x in ti.LEVELS} | {"all levels": "all"}
    blk = data.week
    sides = {"support": [0], "resistance": [1], "both": [0, 1]}
    part = lambda A, j, ss, q, g, bm: (A[j, ss, q, 0, g].sum(0)[bm], A[j, ss, q, 1, g].sum(0)[bm])
    rate = lambda sc: r5(sc[0].sum() / sc[1].sum()) if sc[1].sum() else None

    def run(keys, label, fam):
        cells, pairs = [], []
        for j, ss, q, g, extra in keys:
            twin = []
            for period, bm in (("discovery", blk.disc), ("holdout", ~blk.disc)):
                r, b = part(Ar, j, ss, q, g, bm), part(base, j, ss, q, g, bm)
                c = diff_cell(*r, *b, blk.unit)
                c.update(extra, real=rate(r), placebo=rate(b), signal=names[j], level_group=group_of[names[j]],
                         group=GROUPS[g], period=period, horizon="60m", timeframe="5-min")
                if q:
                    alone = part(base, j, ss, 0, g, bm)
                    c.update(confirmer_alone=r5((rate(b) or 0) - (rate(alone) or 0)) if rate(b) is not None else None,
                             confirmed_share=r5(r[1].sum() / max(part(Ar, j, ss, 0, g, bm)[1].sum(), 1)))
                cells.append(c)
                twin.append(c)
            pairs.append((twin[0], twin[1], extra, GROUPS[g]))
        maxes = []
        for i in range(n_draws):
            bi = (tot - Ap[i]) / (n_draws - 1)
            best = 0.0
            for j, ss, q, g, _ in keys:
                t = diff_t(*part(Ap[i], j, ss, q, g, blk.disc), *part(bi, j, ss, q, g, blk.disc))
                if t is not None:
                    best = max(best, abs(t))
            maxes.append(best)
        finish(pairs, placebo_cut(maxes))
        data.pairs[fam] = pairs
        log(f"{label}: {len(pairs)} discovery cells, cut |t| {placebo_cut(maxes):.2f}, "
            f"{sum(p[0]['verdict'] == 'finding' for p in pairs)} findings")
        return dict(placebo=placebo(maxes, pairs, n_draws), cells=cells)

    main = run([(j, ss, 0, g, dict(target=side)) for j in range(len(names)) for side, ss in sides.items()
                for g in range(3)], "levels", "levels")
    combos = run([(j, [0, 1], q, 0, dict(target="both", context=c["name"]))
                  for j in range(len(names)) for q, c in enumerate(ti.CONFIRMERS, 1)], "level combos", "level_combos")
    outcomes = []
    for j, x in enumerate(ti.LEVELS):
        for p, period in enumerate(("discovery", "holdout")):
            row = dict(signal=x["name"], period=period)
            for src, K in (("real", Kr), ("placebo", Kp)):
                touched, eligible = K[j, p, 3], K[j, p, 4]
                row[src] = dict(touch_rate=r5(touched / eligible) if eligible else None,
                                **{k: r5(K[j, p, i] / touched) if touched else None
                                   for i, k in enumerate(("p_reject", "p_break", "p_timeout"))})
            outcomes.append(row)
    log(f"levels done ({time.time() - t0:.0f}s)")
    return dict(params=dict(window_bars=WIN, bar_minutes=5, band_sd=BAND, confluence_sd=CONFLUENCE, draws=n_draws),
                cells=main["cells"], placebo=main["placebo"], combos=combos, outcomes=outcomes)


# ------------------------------------------------------------------ section G

def leads(data: Data) -> dict:
    """Every discovery finding, its holdout twin (a single test) and, for a
    daily direction or move lead, an out-of-period CRPS test: a drift (or vol
    multiplier) on the signal, fit on discovery, scored on the holdout against
    HAR-RV without it."""
    rows = []
    for fam, pairs in data.pairs.items():
        for disc, hold, sp, gname in pairs:
            if disc.get("verdict") != "finding":
                continue
            row = dict(family=fam, group=gname, discovery=strip(disc), holdout=strip(hold))
            row.update({k: disc[k] for k in ("signal", "target", "horizon", "timeframe", "context", "a", "b") if k in disc})
            if fam in ("direction_daily", "move_daily") and disc.get("target") in ("raw", "move"):
                row["crps"] = crps_test(data, sp, gname, fam == "move_daily")
            rows.append(row)
    survived = sum(r["holdout"].get("verdict") == "finding" and np.sign(r["holdout"]["point"]) == np.sign(r["discovery"]["point"])
                   for r in rows)
    return dict(rows=rows, n=len(rows), survived=survived, expected_false=r5(0.025 * len(rows)))


def strip(c: dict) -> dict:
    return {k: c[k] for k in ("n", "weeks", "block", "null", "point", "lo", "hi", "t", "verdict") if k in c}


def crps_test(data: Data, sp: dict, gname: str, move: bool) -> dict:
    h = sp["horizon"]
    name = sp["signal"]
    s = ti.SIGNALS[name]
    z = (data.q[name].astype(float) if s["kind"] == "continuous" else data.D[name]) * s["sign"]
    y, sig = data.D[f"R{h}"], data.har[h]
    ok = np.isfinite(y) & np.isfinite(sig) & np.isfinite(z) & data.groups[gname][:, None]
    sess = np.nonzero(ok)[1]
    yv, sv, zv = y[ok], sig[ok], z[ok]
    disc = data.disc[sess]
    grid = np.linspace(-0.5, 0.5, 41)
    model = (lambda g: crps(0, sv * np.exp(g * zv), yv)) if move else (lambda g: crps(g * zv * sv, sv, yv))
    best = float(grid[np.argmin([model(g)[disc].mean() for g in grid])])
    c = skill_cell(model(0.0), model(best), sess, data.week if h == 1 else data.month, ~data.disc)
    judge([c], 0.0)
    signed([c])
    c.update(param=r5(best), param_name="vol multiplier exp(g*z)" if move else "drift b*z*sd")
    return c


# ------------------------------------------------------------------ sections A, B

def coverage(data: Data) -> dict:
    """What the snapshot holds, and the 5-minute fold against IB's own daily
    bars wherever both exist (IB's open and close are the official auction
    prints; the fold's are the first and last trades)."""
    check = {k: [] for k in ("open", "high", "low", "close")}
    for si, s in enumerate(data.syms):
        q = data.con.execute(
            "SELECT b.ts, b.open, b.high, b.low, b.close FROM bars b JOIN contracts c ON c.id = b.contract_id "
            "WHERE c.kind = 'stock' AND c.symbol = ? AND b.bar_size = '1d' AND b.data_type = 'trades'", (s,))
        for ts, *ohlc in q:
            i = data.pos.get(dt.date.fromisoformat(ts[:10]))
            if i is None or np.isnan(data.D["close"][si, i]):
                continue
            for k, ib in zip(check, ohlc):
                f = data.D[k][si, i]
                check[k].append((abs(f - ib), abs(f / ib - 1) * 1e4))
    daily = {}
    for k, v in check.items():
        if v:
            a = np.array(v)
            daily[k] = dict(n=len(v), within_cent=r5((a[:, 0] <= 0.0101).mean()), median_bp=r5(np.median(a[:, 1])),
                            p99_bp=r5(np.quantile(a[:, 1], 0.99)), max_bp=r5(a[:, 1].max()))
    for r in data.coverage_rows:
        r["releases"] = data.release_count.get(r["symbol"])
    idx = {}
    for k, (days, _g) in data.index_tapes.items():
        if len(days):
            idx[k] = dict(first=str(days[0]), last=str(days[-1]), sessions=len(days))
    return dict(first=str(data.sess[0]), last=str(data.sess[-1]), sessions=data.n, universe=data.syms,
                split=str(SPLIT), discovery_sessions=int(data.disc.sum()), holdout_sessions=int((~data.disc).sum()),
                symbols=data.coverage_rows, indices=idx, daily_check=daily, macro_releases=data.macro_days,
                min_sessions=MIN_SESSIONS, context_only=list(CONTEXT_ONLY))


def catalog(data: Data) -> dict:
    """The catalog, with how often each signal is active (or each continuous
    signal defined) across the universe: a rare signal cannot carry an interval."""
    out = []
    for ind in ti.CATALOG:
        sigs = []
        for s in ind["signals"]:
            src = data.I if ind["timeframe"] == "I" else data.D
            x = data.q[s["name"]] if s["kind"] == "continuous" else src[s["name"]]
            defined = ~np.isnan(x)
            rate = (x[defined] == 1).mean() if s["kind"] != "continuous" else defined.mean()
            sigs.append(dict(s, active=int((x == 1).sum()) if s["kind"] != "continuous" else int(defined.sum()), rate=r5(rate)))
        out.append(dict(ind, signals=sigs))
    return dict(version=ti.VERSION, post_hoc=ti.POST_HOC, indicators=out, contexts=list(CONTEXTS),
                levels=ti.LEVELS, confirmers=ti.CONFIRMERS,
                pairs={f: [lab for lab, _u, _d in items] for f, items in CORE.items()}, recent=RECENT,
                trend_consensus=list(ti.TREND_CONSENSUS))


# ------------------------------------------------------------------ checks

def twin_check(data: Data) -> None:
    """The numpy statistics must agree with `news_study.stat` on a real cell,
    and the vectorized CRPS with `crps_normal`."""
    sp = spec(data, "above_sma200", "raw", 5, X=data.EX["X5"], blk=data.month)
    S, C = sums(sp["rin"], sp["X"], 1, data.month, data.m)
    s, c = S.sum(0), C.sum(0)
    rows = [dict(day=data.month.days[b], s=s[b], c=c[b]) for b in range(data.month.nb) if c[b] > 0]
    ref = stat(rows, lambda r: r["s"], lambda r: r["c"])
    mine = ratio_cell(s, c, "month")
    assert abs(ref["point"] - mine["point"]) < 1e-5 and abs(ref["t"] - mine["t"]) < 1e-4, (ref, mine)
    width = ref["hi"] - ref["lo"]
    assert abs(ref["lo"] - mine["lo"]) < 0.3 * width and abs(ref["hi"] - mine["hi"]) < 0.3 * width, (ref, mine)
    for mu, sg, y in ((0.0, 1.0, 0.3), (0.01, 0.02, -0.05), (-1.0, 2.0, 4.0)):
        assert abs(crps(mu, sg, y) - crps_normal(mu, sg, y)) < 1e-6
    produced = set(data.D) | set(data.I)
    missing = [n for n in ti.SIGNALS if n not in produced]
    assert not missing, f"catalog signals never computed: {missing}"


# ------------------------------------------------------------------ main

def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--db", default=None, type=Path)
    ap.add_argument("--draws", default=200, type=int)
    ap.add_argument("--out", default=ROOT / "out/ta_study.json", type=Path)
    args = ap.parse_args()
    t0 = time.time()
    data = Data(args.db or Path(default_db()))
    data.pairs = {}
    twin_check(data)
    sections = dict(coverage=coverage(data), catalog=catalog(data))
    sections["direction"] = direction(data, args.draws)
    sections["move_size"] = move_size(data, args.draws)
    sections["entry_exit"] = entry_exit(data, args.draws)
    sections["combinations"] = dict(contexts=contexts(data, args.draws), pairs=confirmation(data, args.draws),
                                    consensus=consensus(data))
    sections["levels"] = levels(data, args.draws)
    sections["leads"] = leads(data)
    commit = subprocess.run(["git", "rev-parse", "--short", "HEAD"], cwd=ROOT, capture_output=True, text=True).stdout.strip()
    report = dict(provenance=dict(
        snapshot=str(data.db_path), commit=commit, generated_at=dt.datetime.now(dt.timezone.utc).isoformat(timespec="seconds"),
        draws=args.draws, catalog_version=ti.VERSION, split=str(SPLIT), seconds=round(time.time() - t0)),
        sections=sections)
    args.out.parent.mkdir(exist_ok=True)
    args.out.write_text(json.dumps(report, indent=1, default=str))
    ld = sections["leads"]
    log(f"leads: {ld['n']} discovery findings, {ld['survived']} survive the holdout (expected false {ld['expected_false']})")
    print(f"wrote {args.out} ({time.time() - t0:.0f}s)")


if __name__ == "__main__":
    main()
