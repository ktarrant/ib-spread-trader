#!/usr/bin/env python3
"""Score headlines with an LLM, the way a live trader would as they arrive.

**Point-in-time by construction.** Headlines are batched by decision window —
each 15-minute RTH slot (09:30-16:00 New York), and one window from the close to
the next 09:30 covering nights and weekends. One call sees every story published
in its window (all symbols, deduplicated across feeds and tickers) plus each
named symbol's stories from the prior 72 hours, and is told "now" is the window
end. Nothing in a call is later than the bar that will read its output, so a
score's `available_at` is its window end.

Telling the model to ignore what it knows does not make it forget, so the
model's training data can still leak into scores of older headlines; see the
contamination checks in news_explore.py. Every call is cached under (window,
chunk, model, prompt version) in a file beside the snapshots — never in the
snapshot — so reruns are free and a newer model is a new record, not a
silent replacement.

    python3 scripts/news_score.py --since 2026-09-01 --limit 3   # a few windows
    python3 scripts/news_score.py --since 2026-06-01 --jobs 4    # resumable
    python3 scripts/news_score.py --dry-run --since 2026-09-15   # print a prompt
"""

from __future__ import annotations

import argparse
import bisect
import collections
import concurrent.futures
import datetime as dt
import difflib
import functools
import json
import sqlite3
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from zoneinfo import ZoneInfo

from news_profile import load, norm, DEDUP_MINUTES
from option_premium import default_db

NY = ZoneInfo("America/New_York")
SCHEMA = Path(__file__).resolve().parent / "news_schema.json"
CACHE = Path.home() / "Documents/Options Database/news-llm.sqlite3"
MODEL = "gpt-5.6-sol"
EFFORT = "low"
PROMPT_VERSION = "v3"
CONTEXT_HOURS = 72
CONTEXT_PER_SYMBOL = 25
CHUNK = 40

INSTRUCTIONS = """\
You score financial news headlines for a trading system, as they arrive. "now" is
the present: treat everything after it as unknown. Use ONLY the headlines given
here. If you recognise a story, do not use anything you know about what happened
after "now". Do not run any commands; answer directly in the required JSON.

For each NEW headline (by id):
- event_type: what kind of news it is. "market_recap" is a report of a price move
  with no new information; "noise" is filings of small holdings, listicles, ads.
- is_repeat: true if it restates a story already in the context or another new item.
- effects: the tracked symbols this news materially affects, each with
  materiality 0-3 (0 none, 1 minor, 2 notable: could move the stock about a normal
  day's range, 3 major: guidance change, M&A, regulatory action, large contract),
  direction -1..1 (expected effect on the price over the horizon, 0 if unclear),
  sentiment -1..1 (the tone of the news for that stock: how good or bad it reads,
  whether or not the price will respond — good news already priced in, or a
  "falls despite strong results" headline, is positive sentiment with a
  non-positive direction),
  vol: whether the next days' moves should be larger than usual for that stock
  (none / elevated / high), and horizon (intraday / days / weeks).
  A routine price-target change or maintained rating is materiality 1 and vol
  none; an upgrade or downgrade is at most 2. vol is elevated or high only when
  this news should make the coming days' moves larger than usual for the stock.
  Include the tagged symbols when materiality >= 1. Include another tracked symbol
  only when the headline clearly bears on it (competitor, customer, supplier,
  sector or macro read-through). Use only symbols from the tracked list.
- future_events: events this headline announces or schedules for a date after
  now (earnings dates, investor days, product launches, regulatory or court
  decisions, shareholder votes, index changes, macro releases). date is
  YYYY-MM-DD, the first day of the period when precision is week/month/quarter.
  Empty when nothing dated is announced.

Then, under "symbols", one entry for each tracked symbol tagged on a NEW
headline: sentiment -1..1, the overall tone of that symbol's news flow across
the context and the new headlines together, weighting recent and material news
more. 0 is neutral or mixed.
"""


def window_end(t: dt.datetime) -> dt.datetime:
    """End of the decision window holding `t` (windows are (start, end])."""
    ny = t.astimezone(NY)
    open_, close = ny.replace(hour=9, minute=30, second=0, microsecond=0), ny.replace(hour=16, minute=0, second=0, microsecond=0)
    if ny.weekday() < 5 and open_ < ny <= close:
        slots = -(-(ny - open_) // dt.timedelta(minutes=15))  # ceil
        return (open_ + slots * dt.timedelta(minutes=15)).astimezone(dt.timezone.utc)
    day = open_ if ny <= open_ else open_ + dt.timedelta(days=1)
    while day.weekday() >= 5:
        day += dt.timedelta(days=1)
    return day.astimezone(dt.timezone.utc)


def build_items(rows: list[dict]) -> list[dict]:
    """Stories across feeds *and* tickers: one item per normalised title within
    DEDUP_MINUTES, carrying every symbol it was tagged with."""
    items, last = [], {}
    for r in sorted(rows, key=lambda r: r["at"]):
        key = norm(r["title"])
        it = last.get(key)
        if it and (r["at"] - it["last"]).total_seconds() <= DEDUP_MINUTES * 60:
            it["symbols"].add(r["symbol"])
            it["providers"].add(r["provider"])
            it["last"] = r["at"]
            continue
        it = dict(key=r["article_id"], at=r["at"], last=r["at"], title=r["title"],
                  symbols={r["symbol"]}, providers={r["provider"]})
        items.append(it)
        last[key] = it
    for it in items:
        it["window"] = window_end(it["at"])
    return items


def distinct(stories: list[dict]) -> list[dict]:
    """Drop reworded reissues (IBD re-titles one story a dozen times a night),
    keeping the latest wording."""
    kept = []
    for s in reversed(stories[-3 * CONTEXT_PER_SYMBOL:]):
        if not any(similar(norm(s["title"]), norm(k["title"])) for k in kept):
            kept.append(s)
    return kept[::-1]


@functools.lru_cache(maxsize=1 << 20)
def similar(a: str, b: str) -> bool:
    return difflib.SequenceMatcher(None, a, b).ratio() > 0.7


def calls(items: list[dict], universe: list[str], keep) -> list[dict]:
    """One call per (window, chunk) whose window end passes `keep`: its new
    items, and each named symbol's prior-72h context from earlier windows."""
    by_window = collections.defaultdict(list)
    by_symbol = collections.defaultdict(list)
    for it in items:
        by_window[it["window"]].append(it)
        for s in it["symbols"]:
            by_symbol[s].append(it)
    times = {s: [c["at"] for c in cs] for s, cs in by_symbol.items()}
    out = []
    for end, new in sorted(by_window.items()):
        if not keep(end):
            continue
        context = {}
        for s in sorted({s for it in new for s in it["symbols"]}):
            lo = bisect.bisect_right(times[s], end - dt.timedelta(hours=CONTEXT_HOURS))
            hi = bisect.bisect_right(times[s], end)
            context[s] = distinct([c for c in by_symbol[s][lo:hi] if c["window"] < end])[-CONTEXT_PER_SYMBOL:]
        assert all(c["at"] <= end and c["window"] < end for cs in context.values() for c in cs), "context from the future"
        assert all(it["at"] <= end for it in new), "item after its window"
        for k in range(0, len(new), CHUNK):
            out.append(dict(window_end=end, chunk=k // CHUNK, items=new[k:k + CHUNK], context=context))
    return out


def stamp(t: dt.datetime) -> str:
    return t.astimezone(dt.timezone.utc).strftime("%Y-%m-%dT%H:%MZ")


def prompt(end, universe, context, chunk) -> str:
    lines = [INSTRUCTIONS, f"now: {stamp(end)} ({end.astimezone(NY):%A})",
             f"tracked symbols: {', '.join(universe)}", "", "Context (earlier headlines, oldest first):"]
    for s, cs in context.items():
        if cs:
            lines.append(f"  {s}:")
            lines += [f"    {stamp(c['at'])} {c['title']}" for c in cs]
    lines += ["", "NEW headlines to score:"]
    lines += [f"[{i}] {stamp(it['at'])} {','.join(sorted(it['symbols']))}: {it['title']}" for i, it in enumerate(chunk)]
    return "\n".join(lines)


def codex(text: str, model: str, effort: str) -> dict:
    with tempfile.TemporaryDirectory() as tmp:
        out = Path(tmp) / "out.json"
        cmd = ["codex", "exec", "--ephemeral", "--skip-git-repo-check", "-s", "read-only", "-C", tmp,
               "-m", model, "-c", f"model_reasoning_effort={effort}",
               "--output-schema", str(SCHEMA), "-o", str(out), "-"]
        res = subprocess.run(cmd, input=text, capture_output=True, text=True, timeout=900)
        if res.returncode != 0 or not out.exists():
            raise RuntimeError(f"codex exited {res.returncode}: {res.stderr[-500:] or res.stdout[-500:]}")
        return json.loads(out.read_text())


def open_cache(path: Path) -> sqlite3.Connection:
    db = sqlite3.connect(path, check_same_thread=False)
    db.execute("""CREATE TABLE IF NOT EXISTS calls (
        window_end TEXT NOT NULL, chunk INTEGER NOT NULL, model TEXT NOT NULL, prompt_version TEXT NOT NULL,
        effort TEXT NOT NULL, items_json TEXT NOT NULL, prompt TEXT NOT NULL, raw_json TEXT NOT NULL,
        seconds REAL NOT NULL, created_at TEXT NOT NULL,
        PRIMARY KEY (window_end, chunk, model, prompt_version))""")
    return db


def _calls(cache: Path, model: str, version: str):
    db = sqlite3.connect(f"file:{cache}?mode=ro", uri=True)
    for end, items, raw in db.execute(
        "SELECT window_end, items_json, raw_json FROM calls WHERE model = ? AND prompt_version = ?", (model, version)
    ):
        yield dt.datetime.fromisoformat(end), json.loads(items), json.loads(raw)


def scored(cache: Path = CACHE, model: str = MODEL, version: str = PROMPT_VERSION) -> list[dict]:
    """Every cached item with its scores; `available_at` is the window end.
    An id the model returned that matches no item is dropped, not trusted."""
    out = []
    for end, items, raw in _calls(cache, model, version):
        for s in raw["items"]:
            try:
                it = items[int(s["id"])]
            except (ValueError, IndexError):
                continue
            out.append(dict(it, available_at=end, event_type=s["event_type"], is_repeat=s["is_repeat"],
                            effects=s["effects"], future_events=s["future_events"]))
    return out


def symbol_sentiment(cache: Path = CACHE, model: str = MODEL, version: str = PROMPT_VERSION) -> list[dict]:
    """The model's read of the tone around each symbol a call named, as of its
    window end. A window split into chunks gives one reading per chunk."""
    return [dict(symbol=s["symbol"], available_at=end, sentiment=s["sentiment"])
            for end, _items, raw in _calls(cache, model, version) for s in raw["symbols"]]


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--db", default=None)
    ap.add_argument("--cache", default=CACHE, type=Path)
    ap.add_argument("--model", default=MODEL)
    ap.add_argument("--effort", default=EFFORT)
    ap.add_argument("--since", type=dt.date.fromisoformat, required=True)
    ap.add_argument("--until", type=dt.date.fromisoformat, default=None)
    ap.add_argument("--limit", type=int, default=None, help="score at most this many calls")
    ap.add_argument("--jobs", type=int, default=2)
    ap.add_argument("--dry-run", action="store_true", help="print the first prompt and the call count")
    args = ap.parse_args()

    snap = sqlite3.connect(f"file:{args.db or default_db()}?mode=ro", uri=True)
    rows = load(snap)
    universe = sorted({r["symbol"] for r in rows})
    items = build_items(rows)
    keep = lambda end: end.date() >= args.since and (args.until is None or end.date() <= args.until)
    todo = calls(items, universe, keep)

    cache = open_cache(args.cache)
    done = {(e, k) for e, k in cache.execute(
        "SELECT window_end, chunk FROM calls WHERE model = ? AND prompt_version = ?", (args.model, PROMPT_VERSION))}
    todo = [c for c in todo if (c["window_end"].isoformat(), c["chunk"]) not in done][:args.limit]
    for c in todo:
        c["prompt"] = prompt(c["window_end"], universe, c["context"], c["items"])
    print(f"{len(items)} stories; {len(todo)} calls to make", file=sys.stderr)
    if args.dry_run:
        print(todo[0]["prompt"] if todo else "(nothing to do)")
        return

    def run(c):
        t0 = time.time()
        for attempt in range(2):
            try:
                return c, codex(c["prompt"], args.model, args.effort), time.time() - t0
            except Exception as e:  # one retry, then report and move on
                err = e
        return c, err, time.time() - t0

    failed = 0
    with concurrent.futures.ThreadPoolExecutor(args.jobs) as pool:
        for n, (c, res, secs) in enumerate(pool.map(run, todo), 1):
            if isinstance(res, Exception):
                failed += 1
                print(f"[{n}/{len(todo)}] {c['window_end']:%Y-%m-%d %H:%M} FAILED: {res}", file=sys.stderr)
                continue
            items_json = json.dumps([dict(key=it["key"], at=it["at"].isoformat(), title=it["title"],
                                          symbols=sorted(it["symbols"]), providers=sorted(it["providers"]))
                                     for it in c["items"]])
            cache.execute("INSERT OR REPLACE INTO calls VALUES (?,?,?,?,?,?,?,?,?,?)", (
                c["window_end"].isoformat(), c["chunk"], args.model, PROMPT_VERSION, args.effort, items_json,
                c["prompt"], json.dumps(res), secs, dt.datetime.now(dt.timezone.utc).isoformat()))
            cache.commit()
            print(f"[{n}/{len(todo)}] {c['window_end']:%Y-%m-%d %H:%M} {len(c['items'])} items {secs:.0f}s",
                  file=sys.stderr)
    print(json.dumps(dict(calls=len(todo), failed=failed, cache=str(args.cache))))


if __name__ == "__main__":
    main()
