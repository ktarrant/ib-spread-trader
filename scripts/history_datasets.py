#!/usr/bin/env python3
"""Write datasets/history.toml and etf-history.toml from a research DB: every
monthly expiry it holds a tradeable chain for, with all.toml's and
etf-all.toml's symbols. The research DB (ib-option-calc's
docs/research-db.md) carries UW's tape back to 2023-11, so these reach years
further than the capture's own chains.

    python scripts/history_datasets.py D:/ib-research/research-YYYY-MM-DD.sqlite3

A monthly expiry is the third Friday, or the Thursday before it when that
Friday is a holiday (2025-04-17, Good Friday). Tradeable is the catalog's
rule: at least 4 paired strikes and 2 sessions, at the 15-minute bars. Only
expiries up to 45 days past the data's last bar are kept, as all.toml keeps
the next monthly: the tape lists LEAP months years out, which the frontier,
trading the front month, never reaches and would only load.
"""
from __future__ import annotations

import pathlib
import sqlite3
import sys
from datetime import date, timedelta

ROOT = pathlib.Path(__file__).resolve().parent.parent


def third_friday(year: int, month: int) -> date:
    first = date(year, month, 1)
    return first + timedelta(days=(4 - first.weekday()) % 7 + 14)


def is_monthly(day: date) -> bool:
    friday = third_friday(day.year, day.month)
    return day == friday or day == friday - timedelta(days=1)


def symbols_of(name: str) -> list[str]:
    text = (ROOT / "datasets" / name).read_text(encoding="utf-8")
    line = next(l for l in text.splitlines() if l.startswith("symbols"))
    return [s.strip().strip('"') for s in line.split("[", 1)[1].rsplit("]", 1)[0].split(",") if s.strip()]


def main() -> int:
    db = sqlite3.connect(pathlib.Path(sys.argv[1]).resolve().as_uri() + "?mode=ro", uri=True)
    tradeable = db.execute(
        "SELECT symbol, expiry FROM chains WHERE minutes = 15 AND paired_strikes >= 4 AND sessions >= 2"
    ).fetchall()
    built = dict(db.execute("SELECT key, value FROM research_meta")).get("snapshot", "?")
    last = db.execute("SELECT MAX(last_t) FROM chains").fetchone()[0]
    horizon = (date(1970, 1, 1) + timedelta(minutes=last) + timedelta(days=45)).isoformat()
    for source, target in (("all.toml", "history.toml"), ("etf-all.toml", "etf-history.toml")):
        symbols = symbols_of(source)
        by_expiry: dict[str, set[str]] = {}
        for symbol, expiry in tradeable:
            if symbol in symbols and expiry <= horizon and is_monthly(date.fromisoformat(expiry)):
                by_expiry.setdefault(expiry, set()).add(symbol)
        # A Thursday counts only when its Friday has no chain (the holiday case).
        expiries = sorted(e for e in by_expiry
                          if date.fromisoformat(e).weekday() == 4
                          or (date.fromisoformat(e) + timedelta(days=1)).isoformat() not in by_expiry)
        (ROOT / "datasets" / target).write_text(
            f"# Every monthly chain a research DB holds for {source}'s symbols, from UW's\n"
            f"# tape (2023-11 on) and the capture: written by scripts/history_datasets.py\n"
            f"# from a build of {built}. Regenerate it rather than editing it.\n"
            f"symbols = [{', '.join(f'\"{s}\"' for s in symbols)}]\n"
            f"expiries = [{', '.join(f'\"{e}\"' for e in expiries)}]\n",
            encoding="utf-8", newline="\n")
        print(f"{target}: {len(symbols)} symbols, {len(expiries)} monthly expiries, "
              f"{expiries[0] if expiries else '-'} to {expiries[-1] if expiries else '-'}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
