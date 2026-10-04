"""Read the LLM headline scores that ib-option-calc's `news-score` caches.

Scoring runs on the capture server after each daily headline sync (see
~/ib-option-calc `src/events/score.rs`); `scripts/fetch_news_cache.sh` copies
its cache (`news-llm.sqlite3`) beside the snapshots. One row per (decision
window, chunk, model, prompt version): `items_json` lists the headlines sent
(index = the id the model answered with), `raw_json` the answer.

A score is known at its window's end (`available_at`), never before. Only the
newest model and prompt version are read — the ones most recently written —
so a re-score never mixes with the run it replaces.
"""

from __future__ import annotations

import datetime as dt
import json
import sqlite3
from pathlib import Path

CACHE = Path.home() / "Documents/Options Database/news-llm.sqlite3"


def _calls(cache: Path):
    db = sqlite3.connect(f"file:{cache}?mode=ro", uri=True)
    latest = db.execute("SELECT model, prompt_version FROM calls ORDER BY created_at DESC LIMIT 1").fetchone()
    if latest is None:
        return
    for end, items, raw in db.execute(
        "SELECT window_end, items_json, raw_json FROM calls WHERE model = ? AND prompt_version = ?", latest
    ):
        yield dt.datetime.fromisoformat(end), json.loads(items), json.loads(raw)


def scored(cache: Path = CACHE) -> list[dict]:
    """Every scored headline; `available_at` is its window's end. An id the
    model returned that matches no headline is dropped, not trusted."""
    out = []
    for end, items, raw in _calls(cache):
        for s in raw["items"]:
            try:
                it = items[int(s["id"])]
            except (ValueError, IndexError):
                continue
            out.append(dict(it, available_at=end, event_type=s["event_type"], is_repeat=s["is_repeat"],
                            effects=s["effects"], future_events=s["future_events"],
                            rationale=s.get("rationale", "")))
    return out


def symbol_sentiment(cache: Path = CACHE) -> list[dict]:
    """The model's read of the tone around each symbol a call named, as of its
    window end. A window split into chunks gives one reading per chunk."""
    return [dict(symbol=s["symbol"], available_at=end, sentiment=s["sentiment"])
            for end, _items, raw in _calls(cache) for s in raw["symbols"]]
