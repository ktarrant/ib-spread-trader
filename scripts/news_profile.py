#!/usr/bin/env python3
"""What the captured headlines can support, before any model reads them.

Prints JSON: per provider, per symbol (stories after cross-provider dedup,
where coverage is complete from, stories per session), and the overlap with
the option and 5-minute stock tapes.

**Coverage.** IB returns only the newest 255 headlines per provider and
symbol, and the capture inserts with `INSERT OR IGNORE`, so a `captured_at`
batch holding 255+ new rows for one (symbol, provider) may hide older ones
behind it. Before that batch's oldest headline the record is incomplete, and
a symbol is complete only from the latest such date across its providers —
earlier bars must read "unknown", never "no news".

    python3 scripts/news_profile.py [--db snapshot.sqlite3]
"""

from __future__ import annotations

import argparse
import collections
import datetime as dt
import json
import re
import sqlite3
import statistics

from option_premium import default_db

CAP = 255
DEDUP_MINUTES = 10


def parse(ts: str) -> dt.datetime:
    return dt.datetime.fromisoformat(ts)


def norm(title: str) -> str:
    return re.sub(r"[^a-z0-9]+", " ", title.lower()).strip()


def load(db) -> list[dict]:
    rows = db.execute(
        """SELECT symbol, event_time, source, title, captured_at,
                  json_extract(detail_json, '$.sentiment'), source_id
           FROM events WHERE kind = 'headline' AND event_time IS NOT NULL
           ORDER BY symbol, event_time"""
    ).fetchall()
    return [dict(symbol=s, at=parse(t), provider=p.removeprefix("ib_news:"), title=ti or "",
                 batch=c[:19], k=k, article_id=a) for s, t, p, ti, c, k, a in rows]


def stories(rows: list[dict]) -> list[list[dict]]:
    """One symbol's headlines grouped into stories: same normalised title
    within DEDUP_MINUTES, whichever provider carried it."""
    out, last = [], {}
    for r in rows:
        key = norm(r["title"])
        s = last.get(key)
        if s and (r["at"] - s[-1]["at"]).total_seconds() <= DEDUP_MINUTES * 60:
            s.append(r)
        else:
            s = [r]
            out.append(s)
            last[key] = s
    return out


def complete_from(rows: list[dict]) -> dt.datetime | None:
    """The latest point where any provider's record may be truncated: before
    the oldest headline of a provider holding near the cap, or before a later
    sync batch that brought a full cap of new rows (a gap since the last sync).
    None when nothing was capped."""
    by = collections.defaultdict(lambda: collections.defaultdict(list))
    for r in rows:
        by[r["provider"]][r["batch"][:16]].append(r["at"])
    cuts = []
    for batches in by.values():
        # ponytail: "near the cap" because syncs overlap (the first ran as one
        # all-provider request), so no single batch shows the 255 it returned.
        if sum(map(len, batches.values())) >= 0.9 * CAP:
            cuts.append(min(min(ts) for ts in batches.values()))
        cuts += [min(ts) for b, ts in sorted(batches.items())[1:] if len(ts) >= CAP]
    return max(cuts) if cuts else None


def tapes(db) -> dict:
    """First/last bar per symbol: option bid bars (any expiry) and 5-min stock."""
    out = collections.defaultdict(dict)
    for sym, kind, lo, hi in db.execute(
        """SELECT c.symbol, c.kind, MIN(b.ts), MAX(b.ts) FROM bars b JOIN contracts c ON c.id = b.contract_id
           WHERE b.data_type = 'bid' AND (c.kind = 'option' OR b.bar_size = '5min')
           GROUP BY c.symbol, c.kind"""
    ):
        out[sym]["option" if kind == "option" else "stock_5min"] = [lo, hi]
    return out


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--db", default=None)
    args = ap.parse_args()
    path = args.db or default_db()
    db = sqlite3.connect(f"file:{path}?mode=ro", uri=True)

    rows = load(db)
    tape = tapes(db)
    providers = collections.defaultdict(lambda: dict(n=0, symbols=set(), first=None, with_sentiment=0))
    for r in rows:
        p = providers[r["provider"]]
        p["n"] += 1
        p["symbols"].add(r["symbol"])
        p["first"] = min(p["first"] or r["at"], r["at"])
        p["with_sentiment"] += r["k"] is not None

    by_symbol = collections.defaultdict(list)
    for r in rows:
        by_symbol[r["symbol"]].append(r)
    symbols = {}
    for sym, rs in sorted(by_symbol.items()):
        st = stories(rs)
        start = complete_from(rs) or min(r["at"] for r in rs)
        per_day = collections.Counter(s[0]["at"].date() for s in st if s[0]["at"] >= start and s[0]["at"].weekday() < 5)
        days = [d for d in (start.date() + dt.timedelta(n) for n in range((rs[-1]["at"].date() - start.date()).days + 1))
                if d.weekday() < 5]
        counts = [per_day.get(d, 0) for d in days]
        symbols[sym] = dict(
            headlines=len(rs),
            stories=len(st),
            complete_from=start.isoformat(),
            capped=complete_from(rs) is not None,
            weekday_stories_median=statistics.median(counts) if counts else 0,
            weekdays_without_news=sum(c == 0 for c in counts),
            weekdays=len(counts),
            tapes=tape.get(sym, {}),
        )

    print(json.dumps(dict(
        snapshot=str(path),
        headlines=len(rows),
        stories=sum(s["stories"] for s in symbols.values()),
        providers={k: dict(v, symbols=len(v["symbols"]), first=v["first"].isoformat())
                   for k, v in sorted(providers.items(), key=lambda kv: -kv[1]["n"])},
        symbols=symbols,
    ), indent=1))


if __name__ == "__main__":
    main()
