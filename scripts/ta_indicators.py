#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = ["numpy", "pandas"]
# ///
"""The technical-indicator catalog for `ta_study.py`, and the indicators.

`CATALOG` is the pre-registration: every indicator, its textbook parameters and
the signals read off it, each with the direction the textbook gives it. It was
fixed before the study's first run; a change after that bumps `VERSION` and is
named in `POST_HOC`, so a reader can tell a definition from a fit.

Every function maps arrays to an array of the same length and returns NaN while
an indicator warms up, never a fabricated 0 (the same rule `implied_vol` keeps
in the engine). Signals are floats: 1.0 when active or fired, 0.0 when not, NaN
when the indicator cannot say. A series must be contiguous: the study calls
these on each symbol's own sessions (or bars), not on the shared calendar.

The engine's RSI comes from the `ta` crate, which smooths with an EMA, and its
ATR is a simple mean of true range. These are the textbook Wilder forms, so the
two differ; whatever this study adopts is ported to Rust and pinned to these.

    uv run scripts/ta_indicators.py        # self-check
"""

from __future__ import annotations

import math

import numpy as np
import pandas as pd
from numpy.lib.stride_tricks import sliding_window_view

VERSION = 2
POST_HOC: list[str] = [
    "v2 (2026-10-05): support/resistance levels (LEVELS) and their confirmers (CONFIRMERS) added after v1's "
    "results were read; every v1 definition is unchanged.",
]


def sig(name: str, kind: str, target: str, sign: int, reading: str) -> dict:
    """kind: state | event | continuous. target: direction | move. sign: the
    textbook reading, +1 bullish (or a larger move), -1 bearish (or smaller).
    A continuous signal is read through quintiles of its own trailing year."""
    return dict(name=name, kind=kind, target=target, sign=sign, reading=reading)


def ind(name, family, era, source, params, timeframe, *signals) -> dict:
    return dict(name=name, family=family, era=era, source=source, params=params, timeframe=timeframe,
                signals=list(signals))


D, I = "D", "I"  # daily bars, 15-minute bars

CATALOG = [
    # ---------------------------------------------------------------- trend
    ind("SMA 50/200", "trend", "classic", "—", "50, 200", D,
        sig("above_sma200", "state", "direction", 1, "close above its 200-day SMA"),
        sig("golden_cross", "event", "direction", 1, "SMA 50 crosses above SMA 200"),
        sig("death_cross", "event", "direction", -1, "SMA 50 crosses below SMA 200")),
    ind("MACD", "trend", "classic", "Appel (1979)", "12, 26, 9", D,
        sig("macd_cross_up", "event", "direction", 1, "MACD line crosses above its signal"),
        sig("macd_cross_dn", "event", "direction", -1, "MACD line crosses below its signal"),
        sig("macd_above_zero", "state", "direction", 1, "MACD line above zero"),
        sig("macd_hist", "continuous", "direction", 1, "histogram over price")),
    ind("ADX / DMI", "trend", "classic", "Wilder (1978)", "14", D,
        sig("di_cross_up", "event", "direction", 1, "+DI crosses above -DI"),
        sig("di_cross_dn", "event", "direction", -1, "+DI crosses below -DI")),
    ind("Parabolic SAR", "trend", "classic", "Wilder (1978)", "0.02, 0.2", D,
        sig("sar_flip_up", "event", "direction", 1, "SAR flips below price"),
        sig("sar_flip_dn", "event", "direction", -1, "SAR flips above price")),
    ind("Ichimoku", "trend", "classic", "Hosoda (1969)", "9, 26, 52", D,
        sig("above_cloud", "state", "direction", 1, "close above the cloud"),
        sig("below_cloud", "state", "direction", -1, "close below the cloud"),
        sig("tk_cross_up", "event", "direction", 1, "tenkan crosses above kijun"),
        sig("tk_cross_dn", "event", "direction", -1, "tenkan crosses below kijun")),
    ind("Donchian channel", "trend", "classic", "Donchian (1960s); the Turtles", "20, 55", D,
        sig("dc20_up", "event", "direction", 1, "first close above the prior 20-day high"),
        sig("dc20_dn", "event", "direction", -1, "first close below the prior 20-day low"),
        sig("dc55_up", "event", "direction", 1, "first close above the prior 55-day high"),
        sig("dc55_dn", "event", "direction", -1, "first close below the prior 55-day low")),
    ind("Aroon", "trend", "classic", "Chande (1995)", "25", D,
        sig("aroon_cross_up", "event", "direction", 1, "Aroon up crosses above Aroon down"),
        sig("aroon_cross_dn", "event", "direction", -1, "Aroon up crosses below Aroon down"),
        sig("aroon_osc", "continuous", "direction", 1, "Aroon up minus down")),
    ind("SuperTrend", "trend", "modern", "Seban (2000s)", "ATR 10, x3", D,
        sig("st_flip_up", "event", "direction", 1, "SuperTrend flips long"),
        sig("st_flip_dn", "event", "direction", -1, "SuperTrend flips short")),
    ind("KAMA / efficiency ratio", "trend", "modern", "Kaufman (1995)", "10, 2, 30", D,
        sig("kama_slope", "continuous", "direction", 1, "one-day change in KAMA"),
        sig("efficiency_ratio", "continuous", "move", 1, "net move over path length, 10 days")),
    ind("Vortex", "trend", "modern", "Botes & Siepman (2010)", "14", D,
        sig("vortex_cross_up", "event", "direction", 1, "VI+ crosses above VI-"),
        sig("vortex_cross_dn", "event", "direction", -1, "VI+ crosses below VI-")),
    ind("Time-series momentum", "trend", "modern", "Moskowitz, Ooi & Pedersen (2012)", "12-1 months", D,
        sig("tsmom_pos", "state", "direction", 1, "return from t-252 to t-21 above zero"),
        sig("tsmom_neg", "state", "direction", -1, "return from t-252 to t-21 below zero"),
        sig("tsmom", "continuous", "direction", 1, "return from t-252 to t-21")),
    ind("Regression slope", "trend", "modern", "—", "20", D,
        sig("regslope_t", "continuous", "direction", 1, "t-statistic of the 20-day log-price slope")),
    # ------------------------------------------------------------- oscillator
    ind("RSI", "oscillator", "classic", "Wilder (1978)", "14, 30/70", D,
        sig("rsi14_lt30", "state", "direction", 1, "RSI below 30, oversold"),
        sig("rsi14_gt70", "state", "direction", -1, "RSI above 70, overbought"),
        sig("rsi14_cross_up30", "event", "direction", 1, "RSI crosses up through 30"),
        sig("rsi14_cross_dn70", "event", "direction", -1, "RSI crosses down through 70"),
        sig("rsi14", "continuous", "direction", -1, "RSI level, read as mean reversion")),
    ind("Stochastic", "oscillator", "classic", "Lane (1950s)", "14, 3, 3; 20/80", D,
        sig("stoch_lt20", "state", "direction", 1, "slow %K below 20"),
        sig("stoch_gt80", "state", "direction", -1, "slow %K above 80"),
        sig("stoch_cross_up", "event", "direction", 1, "%K crosses above %D below 20"),
        sig("stoch_cross_dn", "event", "direction", -1, "%K crosses below %D above 80")),
    ind("Williams %R", "oscillator", "classic", "Williams (1973)", "14; -80/-20", D,
        sig("wr_lt80", "state", "direction", 1, "%R below -80"),
        sig("wr_gt20", "state", "direction", -1, "%R above -20")),
    ind("CCI", "oscillator", "classic", "Lambert (1980)", "20; +/-100", D,
        sig("cci_cross_up100", "event", "direction", 1, "CCI crosses up through +100, Lambert's trend entry"),
        sig("cci_cross_dn100", "event", "direction", -1, "CCI crosses down through -100"),
        sig("cci20", "continuous", "direction", 1, "CCI level")),
    ind("Rate of change", "oscillator", "classic", "—", "10", D,
        sig("roc10", "continuous", "direction", 1, "10-day return")),
    ind("Bollinger %B", "oscillator", "classic", "Bollinger (1980s)", "20, 2", D,
        sig("bb_below", "state", "direction", 1, "close below the lower band"),
        sig("bb_above", "state", "direction", -1, "close above the upper band"),
        sig("bb_pctb", "continuous", "direction", -1, "%B, read as mean reversion")),
    ind("RSI(2)", "oscillator", "modern", "Connors & Alvarez (2008)", "2; 10/90", D,
        sig("rsi2_lt10", "state", "direction", 1, "RSI(2) below 10"),
        sig("rsi2_gt90", "state", "direction", -1, "RSI(2) above 90")),
    ind("ConnorsRSI", "oscillator", "modern", "Connors Research (2012)", "3, 2, 100; 10/90", D,
        sig("crsi_lt10", "state", "direction", 1, "ConnorsRSI below 10"),
        sig("crsi_gt90", "state", "direction", -1, "ConnorsRSI above 90")),
    ind("Internal bar strength", "oscillator", "modern", "—", "0.2/0.8", D,
        sig("ibs_lt20", "state", "direction", 1, "close in the bottom fifth of the day's range"),
        sig("ibs_gt80", "state", "direction", -1, "close in the top fifth of the day's range"),
        sig("ibs", "continuous", "direction", -1, "IBS, read as mean reversion")),
    ind("Fisher transform", "oscillator", "modern", "Ehlers (2002)", "10", D,
        sig("fisher_up", "event", "direction", 1, "Fisher turns up while below zero"),
        sig("fisher_dn", "event", "direction", -1, "Fisher turns down while above zero")),
    # ------------------------------------------------------------- volatility
    ind("Bollinger bandwidth", "volatility", "classic", "Bollinger (1980s)", "20, 2; 126-day low", D,
        sig("bbw_squeeze", "state", "move", 1, "bandwidth at its 6-month low"),
        sig("bbw", "continuous", "move", -1, "bandwidth, read as vol mean reversion")),
    ind("ATR", "volatility", "classic", "Wilder (1978)", "14; 5/50", D,
        sig("atr_pct", "continuous", "move", -1, "ATR(14) over price"),
        sig("atr_ratio", "continuous", "move", -1, "ATR(5) over ATR(50)")),
    ind("Narrow range / inside day", "volatility", "classic", "Crabel (1990)", "NR7, NR4", D,
        sig("nr7", "state", "move", 1, "narrowest range of 7 days"),
        sig("nr4", "state", "move", 1, "narrowest range of 4 days"),
        sig("inside_day", "state", "move", 1, "high and low inside yesterday's")),
    ind("TTM squeeze", "volatility", "modern", "Carter (2005)", "BB 20/2 in KC 20/1.5", D,
        sig("ttm_on", "state", "move", 1, "Bollinger bands inside Keltner"),
        sig("ttm_fire_up", "event", "direction", 1, "squeeze releases, momentum up"),
        sig("ttm_fire_dn", "event", "direction", -1, "squeeze releases, momentum down")),
    ind("Jump share", "volatility", "modern", "Barndorff-Nielsen & Shephard (2004)", "5-min, 20", D,
        sig("jump_share", "continuous", "move", 1, "share of realized variance that bipower calls jumps")),
    ind("Overnight share", "volatility", "modern", "—", "20", D,
        sig("overnight_share", "continuous", "move", 1, "share of variance in the overnight gap")),
    # ------------------------------------------------------------------ volume
    ind("On-balance volume", "volume", "classic", "Granville (1963)", "20", D,
        sig("obv_slope", "continuous", "direction", 1, "20-day OBV change over volume"),
        sig("obv_div_bull", "event", "direction", 1, "20-day low in price, not in OBV"),
        sig("obv_div_bear", "event", "direction", -1, "20-day high in price, not in OBV")),
    ind("Chaikin money flow / A-D", "volume", "classic", "Chaikin (1980s)", "20; +/-0.1", D,
        sig("cmf_gt10", "state", "direction", 1, "CMF above +0.1"),
        sig("cmf_lt10", "state", "direction", -1, "CMF below -0.1"),
        sig("cmf20", "continuous", "direction", 1, "CMF level"),
        sig("ad_slope", "continuous", "direction", 1, "20-day A/D change over volume")),
    ind("Money flow index", "volume", "classic", "Quong & Soudack (1989)", "14; 20/80", D,
        sig("mfi_lt20", "state", "direction", 1, "MFI below 20"),
        sig("mfi_gt80", "state", "direction", -1, "MFI above 80")),
    ind("VWMA - SMA", "volume", "modern", "—", "20", D,
        sig("vwma_gap", "continuous", "direction", 1, "volume-weighted over plain 20-day mean")),
    # ------------------------------------------------------------------ market
    ind("SPY trend", "market", "classic", "—", "200", D,
        sig("spy_above_200", "state", "direction", 1, "SPY above its 200-day SMA")),
    ind("Relative strength vs SPY", "market", "classic", "Levy (1967)", "63", D,
        sig("rs63", "continuous", "direction", 1, "63-day return over SPY's")),
    ind("VIX level", "market", "modern", "Whaley (1993); Cboe", "1-year quintiles", D,
        sig("vix_high", "state", "direction", 1, "VIX in its trailing top fifth, a contrarian buy"),
        sig("vix", "continuous", "direction", 1, "VIX level, contrarian"),
        sig("vix_move", "continuous", "move", 1, "VIX level")),
    ind("VIX term structure", "market", "modern", "Cboe VIX3M, VIX9D", "VIX/VIX3M > 1", D,
        sig("vix_backwardation", "state", "direction", -1, "VIX above VIX3M, risk-off"),
        sig("vix_backwardation_move", "state", "move", 1, "VIX above VIX3M"),
        sig("vix9d_ratio", "continuous", "move", 1, "VIX9D over VIX")),
    ind("VVIX", "market", "modern", "Cboe", "1-year quintiles", D,
        sig("vvix", "continuous", "move", 1, "VVIX level")),
    ind("Breadth", "market", "modern", "—", "share above SMA 50", D,
        sig("breadth50", "continuous", "direction", 1, "share of the universe above its 50-day SMA")),
    ind("Credit trend", "market", "modern", "—", "HYG vs SMA 50", D,
        sig("hyg_above_50", "state", "direction", 1, "HYG above its 50-day SMA")),
    # ---------------------------------------------------------------- calendar
    ind("Day of week", "calendar", "classic", "French (1980)", "next session's weekday", D,
        *(sig(f"next_{d[:3].lower()}", "state", "direction", 1, f"the held session is a {d}")
          for d in ("Monday", "Tuesday", "Wednesday", "Thursday", "Friday"))),
    ind("Turn of month", "calendar", "classic", "Ariel (1987)", "last + first 3 sessions", D,
        sig("turn_of_month", "state", "direction", 1, "the held session is in the turn of the month")),
    ind("Opening gap", "calendar", "classic", "—", "in trailing daily sd", D,
        sig("gap", "continuous", "direction", -1, "today's opening gap, read as a fade")),
    ind("Earnings", "calendar", "modern", "Bernard & Thomas (1989), PEAD", "reaction > 1 sd; 5 sessions", D,
        sig("earnings_soon", "state", "move", 1, "a results reaction within the next 5 sessions"),
        sig("earnings_up", "event", "direction", 1, "reaction session closed up more than 1 sd"),
        sig("earnings_dn", "event", "direction", -1, "reaction session closed down more than 1 sd")),
    ind("Macro release", "calendar", "modern", "events: cpi, jobs_report", "next session", D,
        sig("macro_next", "state", "move", 1, "CPI or payrolls on the next session")),
    # ------------------------------------------------------------- intraday
    ind("RSI (15-min)", "oscillator", "classic", "Wilder (1978)", "14, 30/70", I,
        sig("i_rsi14_lt30", "state", "direction", 1, "RSI below 30"),
        sig("i_rsi14_gt70", "state", "direction", -1, "RSI above 70"),
        sig("i_rsi14_cross_up30", "event", "direction", 1, "RSI crosses up through 30"),
        sig("i_rsi14_cross_dn70", "event", "direction", -1, "RSI crosses down through 70")),
    ind("MACD (15-min)", "trend", "classic", "Appel (1979)", "12, 26, 9", I,
        sig("i_macd_cross_up", "event", "direction", 1, "MACD crosses above its signal"),
        sig("i_macd_cross_dn", "event", "direction", -1, "MACD crosses below its signal"),
        sig("i_macd_hist", "continuous", "direction", 1, "histogram over price")),
    ind("Bollinger %B (15-min)", "oscillator", "classic", "Bollinger (1980s)", "20, 2", I,
        sig("i_bb_below", "state", "direction", 1, "close below the lower band"),
        sig("i_bb_above", "state", "direction", -1, "close above the upper band"),
        sig("i_bb_pctb", "continuous", "direction", -1, "%B, read as mean reversion")),
    ind("SuperTrend (15-min)", "trend", "modern", "Seban (2000s)", "ATR 10, x3", I,
        sig("i_st_flip_up", "event", "direction", 1, "SuperTrend flips long"),
        sig("i_st_flip_dn", "event", "direction", -1, "SuperTrend flips short")),
    ind("Engine trend terms (15-min)", "trend", "modern", "this repo: prevailing 65/13, gap 10/30 at 30-min",
        "SMA 130 slope over 26; SMA 20/60", I,
        sig("i_prevailing", "continuous", "direction", 1, "prevailing-trend slope"),
        sig("i_trend_gap", "continuous", "direction", 1, "fast/slow SMA gap; the engine fades it")),
    ind("Session VWAP distance", "oscillator", "modern", "—", "session", I,
        sig("i_vwap_dist", "continuous", "direction", -1, "distance from session VWAP, read as a fade")),
    ind("Relative volume (15-min)", "volume", "modern", "—", "same slot, 20 sessions", I,
        sig("i_rvol_up", "state", "direction", 1, "volume above 2x its slot median on an up bar"),
        sig("i_rvol_dn", "state", "direction", -1, "volume above 2x its slot median on a down bar"),
        sig("i_rvol", "continuous", "move", 1, "volume over its slot median")),
    ind("First-hour range", "volatility", "modern", "Steidlmayer's initial balance", "vs 20-session median", I,
        sig("i_ib_ratio", "continuous", "move", 1, "first-hour range over its 20-session median")),
    ind("Opening gap (15-min)", "calendar", "classic", "—", "first bar", I,
        sig("i_gap", "continuous", "direction", -1, "the opening gap, read as a fade, first bar only"),
        sig("i_abs_gap", "continuous", "move", 1, "size of the opening gap, first bar only")),
]

# Support and resistance: price levels known before they are tested. Each is
# scored by what happens at its first touch in a session (study section H).
LEVELS = [
    dict(name="pivot_p", group="floor pivots", era="classic", source="floor traders", params="(H+L+C)/3 of the prior session", start="open"),
    dict(name="pivot_r1", group="floor pivots", era="classic", source="floor traders", params="2P - L", start="open"),
    dict(name="pivot_s1", group="floor pivots", era="classic", source="floor traders", params="2P - H", start="open"),
    dict(name="pivot_r2", group="floor pivots", era="classic", source="floor traders", params="P + (H - L)", start="open"),
    dict(name="pivot_s2", group="floor pivots", era="classic", source="floor traders", params="P - (H - L)", start="open"),
    dict(name="prev_high", group="prior session", era="classic", source="—", params="prior session high", start="open"),
    dict(name="prev_low", group="prior session", era="classic", source="—", params="prior session low", start="open"),
    dict(name="prev_close", group="prior session", era="classic", source="—", params="prior session close", start="open"),
    dict(name="fib_38", group="Fibonacci", era="classic", source="—", params="low + 38.2 % of the prior 20-session range", start="open"),
    dict(name="fib_50", group="Fibonacci", era="classic", source="—", params="low + 50 % of the prior 20-session range", start="open"),
    dict(name="fib_62", group="Fibonacci", era="classic", source="—", params="low + 61.8 % of the prior 20-session range", start="open"),
    dict(name="poc", group="volume profile", era="modern", source="Steidlmayer (Market Profile)", params="prior session's point of control, 50 bins", start="open"),
    dict(name="vah", group="volume profile", era="modern", source="Steidlmayer (Market Profile)", params="prior session's 70 % value area high", start="open"),
    dict(name="val", group="volume profile", era="modern", source="Steidlmayer (Market Profile)", params="prior session's 70 % value area low", start="open"),
    dict(name="ib_high", group="initial balance", era="modern", source="Steidlmayer (Market Profile)", params="first-hour high, tested from 10:30", start="10:30"),
    dict(name="ib_low", group="initial balance", era="modern", source="Steidlmayer (Market Profile)", params="first-hour low, tested from 10:30", start="10:30"),
]
# Read at the touch bar's close, before the outcome window opens.
CONFIRMERS = [
    dict(name="rsi", reading="RSI(14) of 5-minute closes below 30 at support, above 70 at resistance"),
    dict(name="rejection_bar", reading="the touch bar closes back on the side it came from"),
    dict(name="volume", reading="touch-bar volume above 1.5x its slot's 20-session median"),
    dict(name="trend", reading="prior close above its 50-day SMA at support, below it at resistance"),
    dict(name="confluence", reading="another level type within 0.1 daily sd"),
]

SIGNALS = {s["name"]: dict(s, indicator=i["name"], family=i["family"], era=i["era"], timeframe=i["timeframe"])
           for i in CATALOG for s in i["signals"]}


# ------------------------------------------------------------------ primitives

def roll(x, n):
    return pd.Series(x).rolling(n, min_periods=n)


def sma(x, n):
    return roll(x, n).mean().to_numpy()


def rstd(x, n, ddof=0):
    return roll(x, n).std(ddof=ddof).to_numpy()


def rsum(x, n):
    return roll(x, n).sum().to_numpy()


def rmax(x, n):
    return roll(x, n).max().to_numpy()


def rmin(x, n):
    return roll(x, n).min().to_numpy()


def shift(x, k=1):
    out = np.full(len(x), np.nan)
    if 0 < k < len(x):
        out[k:] = x[:-k]
    return out


def ema(x, n, alpha=None):
    """EMA seeded with the mean of the first `n` valid values (the textbook
    start); NaN before it. `alpha` 1/n is Wilder's smoothing."""
    x = np.asarray(x, float)
    out = np.full(len(x), np.nan)
    v = np.flatnonzero(~np.isnan(x))
    if len(v) < n:
        return out
    i0 = v[0] + n - 1
    y = x[i0:].copy()
    y[0] = x[v[0]:i0 + 1].mean()
    out[i0:] = pd.Series(y).ewm(alpha=alpha or 2 / (n + 1), adjust=False).mean().to_numpy()
    return out


def wilder(x, n):
    return ema(x, n, 1 / n)


def flag(cond, *deps):
    """A boolean as 1.0/0.0, NaN wherever any input could not say."""
    out = np.asarray(cond, float).copy()
    for d in deps:
        out[np.isnan(d)] = np.nan
    return out


def cross_up(a, b):
    a1, b1 = shift(a), shift(b)
    with np.errstate(invalid="ignore"):
        return flag((a > b) & (a1 <= b1), a, b, a1, b1)


def cross_dn(a, b):
    return cross_up(b, a)


def first(cond_flag):
    """The first bar of a run of 1s."""
    return flag((cond_flag == 1) & (shift(cond_flag) == 0), cond_flag, shift(cond_flag))


def windows(x, n):
    """(len, n) trailing windows, NaN rows before the first full one."""
    out = np.full((len(x), n), np.nan)
    if len(x) >= n:
        out[n - 1:] = sliding_window_view(np.asarray(x, float), n)
    return out


def true_range(h, l, c):
    return np.fmax(h - l, np.fmax(np.abs(h - shift(c)), np.abs(l - shift(c))))


# ------------------------------------------------------------------ indicators

def rsi(c, n):
    d = np.diff(c, prepend=np.nan)
    up, dn = np.where(d > 0, d, 0.0), np.where(d < 0, -d, 0.0)
    up[np.isnan(d)] = dn[np.isnan(d)] = np.nan
    ag, al = wilder(up, n), wilder(dn, n)
    with np.errstate(divide="ignore", invalid="ignore"):
        r = 100 - 100 / (1 + ag / al)
    return np.where((al == 0) & (ag >= 0), 100.0, r)


def bollinger(c, n=20, k=2.0):
    mid, sd = sma(c, n), rstd(c, n)
    return mid - k * sd, mid, mid + k * sd


def macd(c, fast=12, slow=26, n=9):
    line = ema(c, fast) - ema(c, slow)
    signal = ema(line, n)
    return line, signal


def dmi(h, l, c, n=14):
    up, dn = h - shift(h), shift(l) - l
    pdm = np.where((up > dn) & (up > 0), up, 0.0)
    mdm = np.where((dn > up) & (dn > 0), dn, 0.0)
    tr = true_range(h, l, c)
    pdm[0] = mdm[0] = tr[0] = np.nan  # Wilder starts on the first change
    atr = wilder(tr, n)
    with np.errstate(divide="ignore", invalid="ignore"):
        pdi, mdi = 100 * wilder(pdm, n) / atr, 100 * wilder(mdm, n) / atr
        dx = 100 * np.abs(pdi - mdi) / (pdi + mdi)
    return pdi, mdi, wilder(dx, n)


def psar(h, l, step=0.02, cap=0.2, warm=10):
    """Wilder's parabolic SAR; returns 1.0 while long, 0.0 while short."""
    n = len(h)
    long_ = np.full(n, np.nan)
    if n < 3:
        return long_
    up, af, ep, s = h[1] >= h[0], step, (h[1] if h[1] >= h[0] else l[1]), (l[0] if h[1] >= h[0] else h[0])
    for i in range(2, n):
        s = s + af * (ep - s)
        if up:
            s = min(s, l[i - 1], l[i - 2])
            if l[i] < s:
                up, s, ep, af = False, ep, l[i], step
            elif h[i] > ep:
                ep, af = h[i], min(af + step, cap)
        else:
            s = max(s, h[i - 1], h[i - 2])
            if h[i] > s:
                up, s, ep, af = True, ep, h[i], step
            elif l[i] < ep:
                ep, af = l[i], min(af + step, cap)
        long_[i] = float(up)
    long_[:warm] = np.nan
    return long_


def supertrend(h, l, c, n=10, k=3.0):
    """1.0 while long, 0.0 while short."""
    atr = wilder(true_range(h, l, c), n)
    hl2 = (h + l) / 2
    ub, lb = hl2 + k * atr, hl2 - k * atr
    out = np.full(len(c), np.nan)
    fu = fl = np.nan
    d = 1.0
    for i in range(len(c)):
        if np.isnan(atr[i]):
            continue
        if np.isnan(fu):
            fu, fl, d = ub[i], lb[i], float(c[i] >= hl2[i])
        else:
            fu = ub[i] if (ub[i] < fu or c[i - 1] > fu) else fu
            fl = lb[i] if (lb[i] > fl or c[i - 1] < fl) else fl
            d = (0.0 if c[i] < fl else 1.0) if d == 1.0 else (1.0 if c[i] > fu else 0.0)
        out[i] = d
    return out


def aroon(h, l, n=25):
    wh, wl = windows(h, n + 1), windows(l, n + 1)
    ok = ~np.isnan(wh).any(1)
    since_h = np.where(ok, np.argmax(np.nan_to_num(wh[:, ::-1], nan=-np.inf), 1), 0)
    since_l = np.where(ok, np.argmin(np.nan_to_num(wl[:, ::-1], nan=np.inf), 1), 0)
    up, dn = 100 * (n - since_h) / n, 100 * (n - since_l) / n
    up[~ok] = dn[~ok] = np.nan
    return up, dn


def kama(c, n=10, fast=2, slow=30):
    er = efficiency_ratio(c, n)
    f, s = 2 / (fast + 1), 2 / (slow + 1)
    sc = (er * (f - s) + s) ** 2
    out = np.full(len(c), np.nan)
    k = np.nan
    for i in range(len(c)):
        if np.isnan(sc[i]):
            continue
        k = c[i] if np.isnan(k) else k + sc[i] * (c[i] - k)
        out[i] = k
    out[: 2 * n] = np.nan
    return out


def efficiency_ratio(c, n=10):
    with np.errstate(divide="ignore", invalid="ignore"):
        return np.abs(c - shift(c, n)) / rsum(np.abs(np.diff(c, prepend=np.nan)), n)


def vortex(h, l, c, n=14):
    tr = rsum(true_range(h, l, c), n)
    return rsum(np.abs(h - shift(l)), n) / tr, rsum(np.abs(l - shift(h)), n) / tr


def slope_t(y, n=20):
    """t-statistic of an OLS slope over the trailing `n` points."""
    w = windows(y, n)
    x = np.arange(n) - (n - 1) / 2
    sxx = (x * x).sum()
    b = (w - w.mean(1, keepdims=True)) @ x / sxx
    res = w - w.mean(1, keepdims=True) - np.outer(b, x)
    with np.errstate(divide="ignore", invalid="ignore"):
        return b / np.sqrt((res * res).sum(1) / (n - 2) / sxx)


def cci(h, l, c, n=20):
    w = windows((h + l + c) / 3, n)
    m = w.mean(1)
    mad = np.abs(w - m[:, None]).mean(1)
    with np.errstate(divide="ignore", invalid="ignore"):
        return (w[:, -1] - m) / (0.015 * mad)


def stochastic(h, l, c, n=14, k=3, d=3):
    hh, ll = rmax(h, n), rmin(l, n)
    with np.errstate(divide="ignore", invalid="ignore"):
        raw = 100 * (c - ll) / (hh - ll)
    slow = sma(raw, k)
    return raw, slow, sma(slow, d)


def connors_rsi(c):
    d = np.sign(np.diff(c, prepend=np.nan))
    streak = np.zeros(len(c))
    for i in range(1, len(c)):
        streak[i] = (max(streak[i - 1], 0) + 1 if d[i] > 0 else min(streak[i - 1], 0) - 1 if d[i] < 0 else 0)
    roc = c / shift(c) - 1
    w = windows(roc, 101)
    pr = 100 * (w[:, :100] < w[:, 100:]).mean(1)
    pr[np.isnan(w).any(1)] = np.nan
    return (rsi(c, 3) + rsi(streak, 2) + pr) / 3


def fisher(h, l, n=10):
    mid = (h + l) / 2
    hh, ll = rmax(mid, n), rmin(mid, n)
    out = np.full(len(mid), np.nan)
    v = f = 0.0
    for i in range(len(mid)):
        if np.isnan(hh[i]):
            continue
        rng = hh[i] - ll[i]
        v = 0.66 * (((mid[i] - ll[i]) / rng if rng > 0 else 0.5) - 0.5) + 0.67 * v
        v = min(max(v, -0.999), 0.999)
        f = 0.5 * math.log((1 + v) / (1 - v)) + 0.5 * f
        out[i] = f
    out[: 2 * n] = np.nan
    return out


def ttm(h, l, c, n=20):
    lo, mid, hi = bollinger(c, n, 2.0)
    atr = wilder(true_range(h, l, c), n)
    on = flag((hi < mid + 1.5 * atr) & (lo > mid - 1.5 * atr), hi, atr)
    base = c - ((rmax(h, n) + rmin(l, n)) / 2 + mid) / 2
    w = windows(base, n)
    x = np.arange(n) - (n - 1) / 2
    mom = w.mean(1) + (w - w.mean(1, keepdims=True)) @ x / (x * x).sum() * x[-1]  # the fit's last value
    return on, mom


def obv(c, v):
    s = np.sign(np.diff(c, prepend=np.nan))
    s[0] = 0
    return np.cumsum(s * v)


def money_flow_multiplier(h, l, c):
    rng = h - l
    with np.errstate(divide="ignore", invalid="ignore"):
        return np.where(rng > 0, ((c - l) - (h - c)) / rng, 0.0)


def mfi(h, l, c, v, n=14):
    tp = (h + l + c) / 3
    raw = tp * v
    dtp = np.diff(tp, prepend=np.nan)
    pos, neg = np.where(dtp > 0, raw, 0.0), np.where(dtp < 0, raw, 0.0)
    pos[0] = neg[0] = np.nan
    with np.errstate(divide="ignore", invalid="ignore"):
        return 100 - 100 / (1 + rsum(pos, n) / rsum(neg, n))


# ------------------------------------------------------------------ signals

def daily(d: dict) -> tuple[dict, dict]:
    """Every daily signal of one symbol from its own contiguous sessions. `d`
    holds open, high, low, close, volume and the 5-minute-derived rv (intraday
    realized variance), bv (bipower variation) and ov (overnight log return).
    Returns (signals, aux): aux holds the states the study's contexts and
    consensus counts read."""
    o, h, l, c, v = (d[k] for k in ("open", "high", "low", "close", "volume"))
    s, aux = {}, {}
    with np.errstate(divide="ignore", invalid="ignore"):
        sma50, sma200 = sma(c, 50), sma(c, 200)
        s["above_sma200"] = flag(c > sma200, sma200)
        s["golden_cross"], s["death_cross"] = cross_up(sma50, sma200), cross_dn(sma50, sma200)

        line, signal = macd(c)
        s["macd_cross_up"], s["macd_cross_dn"] = cross_up(line, signal), cross_dn(line, signal)
        s["macd_above_zero"] = flag(line > 0, line)
        s["macd_hist"] = (line - signal) / c
        aux["macd_bull"] = flag(line > signal, signal)

        pdi, mdi, adx = dmi(h, l, c)
        s["di_cross_up"], s["di_cross_dn"] = cross_up(pdi, mdi), cross_dn(pdi, mdi)
        aux["adx"], aux["di_bull"] = adx, flag(pdi > mdi, pdi, mdi)

        sar = psar(h, l)
        s["sar_flip_up"], s["sar_flip_dn"] = first(sar), first(flag(sar == 0, sar))
        aux["sar_bull"] = sar

        hh9, ll9, hh26, ll26 = rmax(h, 9), rmin(l, 9), rmax(h, 26), rmin(l, 26)
        tenkan, kijun = (hh9 + ll9) / 2, (hh26 + ll26) / 2
        span_a, span_b = shift((tenkan + kijun) / 2, 26), shift((rmax(h, 52) + rmin(l, 52)) / 2, 26)
        top, bot = np.fmax(span_a, span_b), np.fmin(span_a, span_b)
        s["above_cloud"], s["below_cloud"] = flag(c > top, span_a, span_b), flag(c < bot, span_a, span_b)
        s["tk_cross_up"], s["tk_cross_dn"] = cross_up(tenkan, kijun), cross_dn(tenkan, kijun)

        for n in (20, 55):
            ph, pl = shift(rmax(h, n)), shift(rmin(l, n))
            s[f"dc{n}_up"], s[f"dc{n}_dn"] = first(flag(c > ph, ph)), first(flag(c < pl, pl))

        up, dn = aroon(h, l)
        s["aroon_cross_up"], s["aroon_cross_dn"] = cross_up(up, dn), cross_dn(up, dn)
        s["aroon_osc"] = up - dn
        aux["aroon_bull"] = flag(up > dn, up)

        st = supertrend(h, l, c)
        s["st_flip_up"], s["st_flip_dn"] = first(st), first(flag(st == 0, st))
        aux["st_bull"] = st

        k = kama(c)
        s["kama_slope"] = k / shift(k) - 1
        s["efficiency_ratio"] = efficiency_ratio(c)
        aux["kama_bull"] = flag(s["kama_slope"] > 0, s["kama_slope"])

        vp, vm = vortex(h, l, c)
        s["vortex_cross_up"], s["vortex_cross_dn"] = cross_up(vp, vm), cross_dn(vp, vm)
        aux["vortex_bull"] = flag(vp > vm, vp)

        s["tsmom"] = shift(c, 21) / shift(c, 252) - 1
        s["tsmom_pos"], s["tsmom_neg"] = flag(s["tsmom"] > 0, s["tsmom"]), flag(s["tsmom"] < 0, s["tsmom"])
        s["regslope_t"] = slope_t(np.log(c))

        r14 = rsi(c, 14)
        s["rsi14"] = r14
        s["rsi14_lt30"], s["rsi14_gt70"] = flag(r14 < 30, r14), flag(r14 > 70, r14)
        s["rsi14_cross_up30"] = cross_up(r14, np.full(len(c), 30.0))
        s["rsi14_cross_dn70"] = cross_dn(r14, np.full(len(c), 70.0))

        raw, kk, dd = stochastic(h, l, c)
        s["stoch_lt20"], s["stoch_gt80"] = flag(kk < 20, kk), flag(kk > 80, kk)
        s["stoch_cross_up"] = flag((cross_up(kk, dd) == 1) & (kk < 20), cross_up(kk, dd))
        s["stoch_cross_dn"] = flag((cross_dn(kk, dd) == 1) & (kk > 80), cross_dn(kk, dd))
        wr = raw - 100
        s["wr_lt80"], s["wr_gt20"] = flag(wr < -80, wr), flag(wr > -20, wr)

        cc = cci(h, l, c)
        s["cci20"] = cc
        s["cci_cross_up100"] = cross_up(cc, np.full(len(c), 100.0))
        s["cci_cross_dn100"] = cross_dn(cc, np.full(len(c), -100.0))
        s["roc10"] = c / shift(c, 10) - 1

        lo, mid, hi = bollinger(c)
        s["bb_below"], s["bb_above"] = flag(c < lo, lo), flag(c > hi, hi)
        s["bb_pctb"] = (c - lo) / (hi - lo)
        bw = (hi - lo) / mid
        s["bbw"] = bw
        s["bbw_squeeze"] = flag(bw <= rmin(bw, 126), rmin(bw, 126))

        r2 = rsi(c, 2)
        s["rsi2_lt10"], s["rsi2_gt90"] = flag(r2 < 10, r2), flag(r2 > 90, r2)
        cr = connors_rsi(c)
        s["crsi_lt10"], s["crsi_gt90"] = flag(cr < 10, cr), flag(cr > 90, cr)

        ibs = np.where(h > l, (c - l) / (h - l), np.nan)
        s["ibs"] = ibs
        s["ibs_lt20"], s["ibs_gt80"] = flag(ibs < 0.2, ibs), flag(ibs > 0.8, ibs)

        f = fisher(h, l)
        f1, f2 = shift(f), shift(f, 2)
        s["fisher_up"] = flag((f > f1) & (f1 <= f2) & (f < 0), f, f1, f2)
        s["fisher_dn"] = flag((f < f1) & (f1 >= f2) & (f > 0), f, f1, f2)

        tr = true_range(h, l, c)
        s["atr_pct"] = wilder(tr, 14) / c
        s["atr_ratio"] = wilder(tr, 5) / wilder(tr, 50)
        rng = h - l
        s["nr7"], s["nr4"] = flag(rng <= rmin(rng, 7), rmin(rng, 7)), flag(rng <= rmin(rng, 4), rmin(rng, 4))
        s["inside_day"] = flag((h <= shift(h)) & (l >= shift(l)), shift(h))

        on, mom = ttm(h, l, c)
        fire = flag((shift(on) == 1) & (on == 0), on, shift(on), mom)
        s["ttm_on"] = on
        s["ttm_fire_up"], s["ttm_fire_dn"] = flag((fire == 1) & (mom > 0), fire), flag((fire == 1) & (mom < 0), fire)

        rv, bv, ov = d["rv"], d["bv"], d["ov"]
        s["jump_share"] = rsum(np.fmax(rv - bv, 0), 20) / rsum(rv, 20)
        s["overnight_share"] = rsum(ov * ov, 20) / rsum(ov * ov + rv, 20)

        ob = obv(c, v)
        vol20 = rsum(v, 20)
        s["obv_slope"] = (ob - shift(ob, 20)) / vol20
        s["obv_div_bull"] = flag((c <= rmin(c, 20)) & (ob > rmin(ob, 20)), rmin(c, 20))
        s["obv_div_bear"] = flag((c >= rmax(c, 20)) & (ob < rmax(ob, 20)), rmax(c, 20))
        mfm = money_flow_multiplier(h, l, c)
        cmf = rsum(mfm * v, 20) / vol20
        s["cmf20"] = cmf
        s["cmf_gt10"], s["cmf_lt10"] = flag(cmf > 0.1, cmf), flag(cmf < -0.1, cmf)
        ad = np.cumsum(mfm * v)
        s["ad_slope"] = (ad - shift(ad, 20)) / vol20
        m = mfi(h, l, c, v)
        s["mfi_lt20"], s["mfi_gt80"] = flag(m < 20, m), flag(m > 80, m)
        s["vwma_gap"] = (rsum(c * v, 20) / vol20 - sma(c, 20)) / sma(c, 20)

        aux.update(above_sma200=s["above_sma200"], above_cloud=s["above_cloud"], tsmom_bull=s["tsmom_pos"],
                   sma50=sma50, osc_bull=[s[k] for k in ("rsi14_lt30", "stoch_lt20", "wr_lt80", "bb_below", "rsi2_lt10",
                                                          "crsi_lt10", "ibs_lt20", "mfi_lt20")],
                   osc_bear=[s[k] for k in ("rsi14_gt70", "stoch_gt80", "wr_gt20", "bb_above", "rsi2_gt90",
                                            "crsi_gt90", "ibs_gt80", "mfi_gt80")])
        aux["osc_bull"].append(flag(cc < -100, cc))
        aux["osc_bear"].append(flag(cc > 100, cc))
    return s, aux


TREND_CONSENSUS = ("above_sma200", "macd_bull", "st_bull", "sar_bull", "above_cloud", "aroon_bull",
                   "di_bull", "kama_bull", "tsmom_bull", "vortex_bull")


def intraday(b: dict) -> dict:
    """Signals read off one symbol's contiguous 15-minute bars (open, high,
    low, close, volume), continuous across sessions like the engine's."""
    o, h, l, c = (b[k] for k in ("open", "high", "low", "close"))
    s = {}
    with np.errstate(divide="ignore", invalid="ignore"):
        r14 = rsi(c, 14)
        s["i_rsi14_lt30"], s["i_rsi14_gt70"] = flag(r14 < 30, r14), flag(r14 > 70, r14)
        s["i_rsi14_cross_up30"] = cross_up(r14, np.full(len(c), 30.0))
        s["i_rsi14_cross_dn70"] = cross_dn(r14, np.full(len(c), 70.0))
        line, signal = macd(c)
        s["i_macd_cross_up"], s["i_macd_cross_dn"] = cross_up(line, signal), cross_dn(line, signal)
        s["i_macd_hist"] = (line - signal) / c
        lo, _mid, hi = bollinger(c)
        s["i_bb_below"], s["i_bb_above"] = flag(c < lo, lo), flag(c > hi, hi)
        s["i_bb_pctb"] = (c - lo) / (hi - lo)
        st = supertrend(h, l, c)
        s["i_st_flip_up"], s["i_st_flip_dn"] = first(st), first(flag(st == 0, st))
        p = sma(c, 130)
        s["i_prevailing"] = (p - shift(p, 26)) / 26 / c
        s["i_trend_gap"] = (sma(c, 20) - sma(c, 60)) / sma(c, 60)
    return s


def market(spy: np.ndarray, vix, vix3m, vix9d, vvix, hyg, closes: np.ndarray) -> dict:
    """Signals shared by every symbol, on the study's calendar (NaN where a
    series has no print). `closes` is (symbols, sessions)."""
    s = {}
    with np.errstate(divide="ignore", invalid="ignore"):
        s["spy_above_200"] = flag(spy > sma(spy, 200), sma(spy, 200))
        s["vix"] = s["vix_move"] = vix
        q80 = roll(vix, 252).quantile(0.8).to_numpy()
        s["vix_high"] = flag(vix > q80, q80)
        ratio = vix / vix3m
        s["vix_backwardation"] = s["vix_backwardation_move"] = flag(ratio > 1, ratio)
        s["vix9d_ratio"] = vix9d / vix
        s["vvix"] = vvix
        s["hyg_above_50"] = flag(hyg > sma(hyg, 50), sma(hyg, 50))
        above = np.array([flag(c > sma(c, 50), sma(c, 50)) for c in closes])
        s["breadth50"] = np.where(np.isnan(above).all(0), np.nan, np.nanmean(above, 0))
    return s


def pivots(h, l, c) -> dict:
    """Floor pivots for the *next* session from one session's high, low, close."""
    p = (h + l + c) / 3
    return dict(pivot_p=p, pivot_r1=2 * p - l, pivot_s1=2 * p - h, pivot_r2=p + (h - l), pivot_s2=p - (h - l))


def fibs(h, l, n=20) -> dict:
    """38.2 / 50 / 61.8 % of the trailing n-session range, through this session."""
    hh, ll = rmax(h, n), rmin(l, n)
    return {f"fib_{round(100 * f)}": ll + f * (hh - ll) for f in (0.382, 0.5, 0.618)}


def value_area(h, l, v, bins=50, share=0.70):
    """(POC, VAH, VAL) of one session's bars: each bar's volume spread evenly
    over the price range it traded, the POC the busiest bin, the value area
    grown from it by the busier neighbour until it holds `share`."""
    ok = ~np.isnan(h) & ~np.isnan(l) & (np.nan_to_num(v) > 0)
    if ok.sum() < 2:
        return np.nan, np.nan, np.nan
    h, l, v = h[ok], l[ok], v[ok]
    lo, hi = l.min(), h.max()
    if hi <= lo:
        return np.nan, np.nan, np.nan
    edges = np.linspace(lo, hi, bins + 1)
    eps = (hi - lo) * 1e-6
    a, b = l[:, None] - eps, h[:, None] + eps
    ov = np.clip(np.minimum(b, edges[1:]) - np.maximum(a, edges[:-1]), 0, None)
    vol = (ov / (b - a) * v[:, None]).sum(0)
    k = i = j = int(np.argmax(vol))
    tot, need = vol[i], share * vol.sum()
    while tot < need and (i > 0 or j < bins - 1):
        down = vol[i - 1] if i > 0 else -1.0
        up = vol[j + 1] if j < bins - 1 else -1.0
        if up >= down:
            j += 1
            tot += vol[j]
        else:
            i -= 1
            tot += vol[i]
    return (edges[k] + edges[k + 1]) / 2, edges[j + 1], edges[i]


# ------------------------------------------------------------------ check

def erf(x):
    """Abramowitz & Stegun 7.1.26, |error| < 1.5e-7: numpy has no erf."""
    t = 1 / (1 + 0.3275911 * np.abs(x))
    y = 1 - t * (0.254829592 + t * (-0.284496736 + t * (1.421413741 + t * (-1.453152027 + t * 1.061405429)))) \
        * np.exp(-x * x)
    return np.sign(x) * y


def _check() -> None:
    rng = np.random.default_rng(7)
    x = 100 + np.cumsum(rng.normal(0, 1, 400))

    # EMA: seeded with the first n's mean, then the recursion.
    n, ref = 10, np.full(400, np.nan)
    ref[n - 1] = x[:n].mean()
    for i in range(n, 400):
        ref[i] = ref[i - 1] + 2 / (n + 1) * (x[i] - ref[i - 1])
    assert np.allclose(ema(x, n), ref, equal_nan=True)

    # RSI: Wilder's slow loop; 100 on a rising tape; first value after n changes.
    d = np.diff(x)
    ag, al = d[:14].clip(0).mean(), (-d[:14]).clip(0).mean()
    for i in range(14, len(d)):
        ag, al = (ag * 13 + max(d[i], 0)) / 14, (al * 13 + max(-d[i], 0)) / 14
    assert abs(rsi(x, 14)[-1] - (100 - 100 / (1 + ag / al))) < 1e-9
    assert np.isnan(rsi(x, 14)[13]) and not np.isnan(rsi(x, 14)[14])
    assert rsi(np.arange(1.0, 40), 14)[-1] == 100

    # %B is 1 exactly on the upper band.
    lo, mid, hi = bollinger(x)
    assert abs(((hi - lo) / (hi - lo))[-1] - 1) < 1e-12

    # A V: one flip up for SAR and SuperTrend, at the turn not before.
    v = np.r_[np.linspace(100, 80, 60), np.linspace(80, 110, 60)]
    hv, lv = v + 0.5, v - 0.5
    assert np.nansum(first(psar(hv, lv))) == 1 and np.nanargmax(first(psar(hv, lv))) > 60
    assert np.nansum(first(supertrend(hv, lv, v))) == 1

    # Breakouts fire once per run; Aroon up is 100 on a new high.
    assert np.nansum(first(flag(v > shift(rmax(hv, 20)), shift(rmax(hv, 20))))) == 1
    assert aroon(hv, lv)[0][-1] == 100

    # Cross: a touch is not a crossing; the warm-up is not a signal.
    a = np.array([np.nan, 1, 2, 3, 3, 2, 3, 4.0])
    assert list(np.nan_to_num(cross_up(a, np.full(8, 3.0)), nan=-1)) == [-1, -1, 0, 0, 0, 0, 0, 1]

    # A session that trades mostly at 10 has its POC there, inside its value area.
    hh = np.r_[np.full(20, 10.05), np.full(5, 10.2), 11.0, 12.0]
    ll = np.r_[np.full(20, 9.95), np.full(5, 9.8), 10.5, 11.5]
    poc, vah, val = value_area(hh, ll, np.r_[np.full(25, 100.0), 5, 5])
    assert abs(poc - 10) < 0.05 and 9.8 <= val < 10 < vah <= 10.25, (poc, vah, val)
    assert abs(pivots(12.0, 8.0, 10.0)["pivot_r1"] - 12.0) < 1e-12

    z = np.linspace(-4, 4, 81)
    assert np.max(np.abs(erf(z) - np.array([math.erf(t) for t in z]))) < 2e-7
    for i in CATALOG:
        assert i["timeframe"] in (D, I) and i["family"] in (
            "trend", "oscillator", "volatility", "volume", "market", "calendar")
    print(f"ta_indicators: {len(CATALOG)} indicators, {len(SIGNALS)} signals, self-check passed")


if __name__ == "__main__":
    _check()
