#!/usr/bin/env python3
"""Copy-trade ARK's daily ETF trades in plain stock, on one cash account.

A one-off for browsing in the viz (`out/ark_copy/`), not a frontier run, so
`scripts/run_frontier.sh` moves it to `out_archive/` on its next pass.

Rules. ARK's funds trade during session T and publish after its close, so a
day's net flow on a symbol (buys minus sells, in fund weight, across every ARK
fund) is acted on at the next session's opening quote, crossing the spread:
- net buy: buy one tranche (`--tranche` dollars, whole shares) at the ask, if
  the cash is there; otherwise the signal is counted as capital-limited.
- net sell: sell the oldest open lot of that symbol at the bid. ARK sells what
  it already held; we start flat, so a sell with no lot is counted, not shorted.
Each lot is one trade. Lots ARK never sold close at the bid on the symbol's
last captured bar, as `end_of_data`. Only cohort names have a quote tape, so
ARK's trades in everything else are invisible here. Commission as in
`src/intraday.rs`: $0.005 a share, $1 minimum.

    python3 scripts/ark_copy.py [--db snapshot] [--cash 100000] [--tranche 2500]
"""

from __future__ import annotations

import argparse
import bisect
import collections
import datetime as dt
import json
import sqlite3
from pathlib import Path

from option_premium import ETFS, default_db

OUT = Path(__file__).resolve().parent.parent / "out" / "ark_copy"
STAMP = "%Y-%m-%dT%H:%M:%S+00:00"


def commission(shares: int) -> float:
    return max(shares * 0.005, 1.0)


def tape(db) -> dict[str, dict]:
    """Per symbol: its 5-minute bar starts, and per session the first
    uncrossed opening quote (ts, bid, ask) and the last bid close."""
    q = collections.defaultdict(dict)
    for sym, ts, kind, o, c in db.execute(
        """SELECT c.symbol, ts, data_type, open, close FROM bars b JOIN contracts c ON c.id = b.contract_id
           WHERE c.kind = 'stock' AND bar_size = '5min' AND data_type IN ('bid', 'ask') ORDER BY ts"""
    ):
        q[sym].setdefault(ts, {})[kind] = (o, c)
    out = {}
    for sym, bars in q.items():
        if sym in ETFS:
            continue
        sessions = {}
        for ts, b in bars.items():
            if len(b) < 2:
                continue
            day = dt.date.fromisoformat(ts[:10])
            s = sessions.setdefault(day, {})
            if "open" not in s and 0 < b["bid"][0] < b["ask"][0]:
                s["open"] = (dt.datetime.fromisoformat(ts), b["bid"][0], b["ask"][0])
            s["close"] = (dt.datetime.fromisoformat(ts) + dt.timedelta(minutes=5), b["bid"][1])
        out[sym] = dict(ts=[dt.datetime.fromisoformat(t) for t in bars], sessions={d: s for d, s in sessions.items() if "open" in s})
    return out


def signals(db, symbols) -> list[tuple]:
    flow, funds = collections.defaultdict(float), collections.defaultdict(set)
    for sym, day, kind, detail in db.execute(
        "SELECT symbol, event_date, kind, detail_json FROM events WHERE kind IN ('ark_buy', 'ark_sell')"
    ):
        if sym in symbols:
            d = json.loads(detail)
            flow[sym, day] += (1 if kind == "ark_buy" else -1) * d["etf_percent"]
            funds[sym, day].add(d["fund"])
    return [(dt.date.fromisoformat(d), s, f, "+".join(sorted(funds[s, d]))) for (s, d), f in flow.items() if f]


def run(db, cash0: float, tranche: float) -> tuple[dict, list[tuple]]:
    data = tape(db)
    start = min(min(v["sessions"]) for v in data.values())
    census = collections.Counter()
    due = collections.defaultdict(list)  # fill date -> [(symbol, flow, funds, signal day)]
    for day, sym, f, funds in signals(db, data):
        if day < start - dt.timedelta(days=1):
            continue  # before the quote tape: nothing to copy against
        nxt = [d for d in sorted(data[sym]["sessions"]) if d > day]
        if not nxt or (nxt[0] - day).days > 4:
            census["outside_quote_tape"] += 1
            continue
        due[nxt[0]].append((sym, f, funds, day))

    cash, lots, trades, curve = cash0, collections.defaultdict(list), [], []
    sym_stats = {s: collections.Counter() for s in data}
    sym_path = collections.defaultdict(list)  # per symbol, its cumulative P&L at each close
    days = sorted({d for v in data.values() for d in v["sessions"]})

    def close_lot(sym, lot, when, px, reason):
        nonlocal cash
        fee = commission(lot["shares"])
        proceeds = lot["shares"] * px
        cash += proceeds - fee
        series = data[sym]["ts"]
        trades.append(dict(
            symbol=sym,
            description=f"+{lot['shares']} {sym} @ {lot['px']:.2f} → {px:.2f} ({lot['funds']} bought {lot['signal']})",
            opened_at=lot["at"].strftime(STAMP), closed_at=when.strftime(STAMP),
            bars_held=bisect.bisect_left(series, when) - bisect.bisect_left(series, lot["at"]),
            entry_debit=round(lot["shares"] * lot["px"], 2), exit_credit=round(proceeds, 2),
            commission=round(lot["fee"] + fee, 2),
            pnl=round(proceeds - lot["shares"] * lot["px"] - lot["fee"] - fee, 2),
            exit_reason=reason,
        ))

    curve.append((days[0], dt.datetime.combine(days[0], dt.time(13, 30), dt.timezone.utc), cash0, 0.0))
    for day in days:
        orders = sorted(due.pop(day, []), key=lambda o: o[1])  # sells first, then the largest buys
        for sym, f, funds, signal in orders:
            sym_stats[sym]["triggers"] += 1
            at, bid, ask = data[sym]["sessions"][day]["open"]
            if f < 0:
                if not lots[sym]:
                    census["sell_with_nothing_held"] += 1
                    sym_stats[sym]["rejections"] += 1
                    continue
                close_lot(sym, lots[sym].pop(0), at, bid, "ark_sell")
            else:
                shares = int(tranche // ask)
                fee = commission(shares)
                if shares == 0 or shares * ask + fee > cash:
                    census["capital_limited"] += 1
                    sym_stats[sym]["rejections"] += 1
                    continue
                cash -= shares * ask + fee
                lots[sym].append(dict(shares=shares, px=ask, fee=fee, at=at, funds=funds, signal=signal))
        # Mark at each symbol's latest bid close; a symbol whose tape has ended
        # closes its lots there.
        value = invested = 0.0
        for sym, held in lots.items():
            sess = data[sym]["sessions"]
            last = max(d for d in sess if d <= day) if held else None
            if held and last == max(sess) and day >= last:
                when, bid = sess[last]["close"]
                for lot in held:
                    close_lot(sym, lot, when, bid, "end_of_data")
                held.clear()
                continue
            for lot in held:
                value += lot["shares"] * sess[last]["close"][1]
                invested += lot["shares"] * lot["px"]
        when = max(v["sessions"][day]["close"][0] for v in data.values() if day in v["sessions"])
        curve.append((day, when, cash + value, invested))
        for sym in data:
            realized = sum(t["pnl"] for t in trades if t["symbol"] == sym)
            sess = data[sym]["sessions"]
            prior = [d for d in sess if d <= day]
            open_pnl = sum(l["shares"] * (sess[max(prior)]["close"][1] - l["px"]) - l["fee"] for l in lots[sym]) if prior else 0
            sym_path[sym].append(realized + open_pnl)

    trades.sort(key=lambda t: (t["opened_at"], t["symbol"]))
    for i, t in enumerate(trades, 1):
        t["id"] = i

    end = curve[-1][2]
    wins = sum(t["pnl"] > 0 for t in trades)
    reasons = collections.Counter(t["exit_reason"] for t in trades)
    spy = spy_return(db, days[0], days[-1])
    report = dict(
        strategy="ARK copy-trade (stock)",
        expiry="stock",
        expiries=["stock"],
        bar_minutes=5,
        labels={"signal": "ark daily trades", "portfolio": f"${cash0:,.0f} cash, ${tranche:,.0f} tranches"},
        config=dict(mode="ark_copy", initial_cash=cash0, tranche=tranche, fill="next session's opening quote, crossed",
                    sell_rule="oldest lot on an ARK net-sell day", commission="$0.005/share, $1 min"),
        totals=dict(
            total_pnl=round(end - cash0, 2), traded_pnl=round(sum(t["pnl"] for t in trades if t["exit_reason"] == "ark_sell"), 2),
            settled_pnl=round(sum(t["pnl"] for t in trades if t["exit_reason"] == "end_of_data"), 2),
            return_pct=(end / cash0 - 1) * 100, trades=len(trades),
            win_rate=wins / len(trades) * 100 if trades else None,
            average_trade_pnl=sum(t["pnl"] for t in trades) / len(trades) if trades else None,
            max_drawdown=max_dd([c[2] for c in curve]),
            commission_paid=round(sum(t["commission"] for t in trades), 2),
            capital_deployed=cash0, exits_by_reason=dict(reasons), census=dict(census),
        ),
        symbols=[dict(
            symbol=s, trades=sum(t["symbol"] == s for t in trades),
            pnl=round(sum(t["pnl"] for t in trades if t["symbol"] == s), 2),
            traded_pnl=round(sum(t["pnl"] for t in trades if t["symbol"] == s and t["exit_reason"] == "ark_sell"), 2),
            settled_pnl=round(sum(t["pnl"] for t in trades if t["symbol"] == s and t["exit_reason"] == "end_of_data"), 2),
            max_drawdown=max_dd([cash0 + x for x in sym_path[s]]),
            triggers=sym_stats[s]["triggers"], rejections=sym_stats[s]["rejections"],
        ) for s in sorted(data)],
        trades=trades,
        caveats=[
            f"{reasons['end_of_data']} of {len(trades)} lots were never sold by ARK inside the window and close at "
            "the bid on their last captured bar (`end_of_data`) — most of the result is marked, not traded.",
            "Only cohort names carry a quote tape; ARK's trades in every other name are not copied.",
            f"We start flat: {census['sell_with_nothing_held']} ARK sell signals found nothing to sell.",
            f"{census['capital_limited']} buy signals skipped for lack of cash.",
            f"SPY over the same sessions: {spy:+.2f}%." if spy is not None else "No SPY tape for a benchmark.",
            f"{len(trades)} trades over {len(days)} sessions: descriptive, not evidence (see scripts/ark_study.py).",
        ],
    )
    return report, curve


def max_dd(xs: list[float]) -> float:
    peak, worst = xs[0], 0.0
    for x in xs:
        peak = max(peak, x)
        worst = max(worst, (peak - x) / peak)
    return worst


def spy_return(db, first: dt.date, last: dt.date) -> float | None:
    rows = db.execute(
        """SELECT b.data_type, ts, open, close FROM bars b JOIN contracts c ON c.id = b.contract_id
           WHERE c.symbol = 'SPY' AND c.kind = 'stock' AND bar_size = '5min' AND data_type IN ('bid', 'ask')
           AND ts >= ? AND ts < ? ORDER BY ts""", (first.isoformat(), (last + dt.timedelta(days=1)).isoformat())
    ).fetchall()
    ask = next((o for k, _, o, _ in rows if k == "ask"), None)
    bid = next((c for k, _, _, c in reversed(rows) if k == "bid"), None)
    return (bid / ask - 1) * 100 if ask and bid else None


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--db", default=default_db())
    ap.add_argument("--cash", type=float, default=100_000)
    ap.add_argument("--tranche", type=float, default=2_500)
    a = ap.parse_args()
    db = sqlite3.connect(f"file:{a.db}?mode=ro", uri=True)
    report, curve = run(db, a.cash, a.tranche)
    OUT.mkdir(parents=True, exist_ok=True)
    (OUT / "backtest.json").write_text(json.dumps(report, indent=2, default=str))
    with open(OUT / "equity.csv", "w") as f:
        f.write("ts,equity,invested\n")
        f.writelines(f"{when.strftime(STAMP)},{eq:.2f},{inv:.2f}\n" for _, when, eq, inv in curve)
    print(json.dumps({k: report["totals"][k] for k in ("total_pnl", "return_pct", "trades", "win_rate", "max_drawdown",
                                                       "exits_by_reason", "census")}, indent=2))


if __name__ == "__main__":
    main()
