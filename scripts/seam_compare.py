#!/usr/bin/env python3
"""Compare a quick check's runs: IB capture alone against the full research DB.

    python scripts/seam_compare.py out_check/<stamp> [--against out_check/<older>]

For each strategy in the check (`<strategy>-ib/` and `<strategy>-full/`), the
average trade P&L and the trade count:

- inside the IB run's own entry window (its first to last `opened_at`), for
  both runs. Inside that window both read the same chains, so the gap is what
  the tape's quotes do to the result;
- for the full run as a whole, and split at the seam (2026-06-17, the first
  session with IB's own option quotes in the research DB).

The previous check (by name, else --against) is printed under each strategy,
so a change's effect is the difference between the two lines. Stdlib only.
"""
from __future__ import annotations

import argparse
import json
from pathlib import Path

SEAM = "2026-06-17"


def trades(run: Path) -> list[dict] | None:
    f = run / "backtest.json"
    if not f.exists():
        return None
    return json.loads(f.read_text(encoding="utf-8"))["trades"]


def avg(rows: list[dict]) -> str:
    if not rows:
        return f"{'-':>13}"
    return f"{sum(t['pnl'] for t in rows) / len(rows):+7.0f} ({len(rows):3d})"


def line(check: Path, strategy: str) -> str | None:
    ib, full = trades(check / f"{strategy}-ib"), trades(check / f"{strategy}-full")
    if ib is None or full is None:
        return None
    if ib:
        lo, hi = min(t["opened_at"] for t in ib), max(t["opened_at"] for t in ib)
        window = [t for t in full if lo <= t["opened_at"] <= hi]
    else:
        window = []
    tape = [t for t in full if t["opened_at"][:10] < SEAM]
    era = [t for t in full if t["opened_at"][:10] >= SEAM]
    return f"{avg(ib)}   {avg(window)}   {avg(full)}   {avg(tape)}   {avg(era)}"


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("check", type=Path)
    ap.add_argument("--against", type=Path)
    args = ap.parse_args()
    check = args.check
    against = args.against
    if against is None:
        older = sorted(p for p in check.parent.iterdir() if p.is_dir() and p.name < check.name)
        against = older[-1] if older else None
    strategies = sorted(p.name[:-3] for p in check.iterdir() if p.is_dir() and p.name.endswith("-ib"))
    print(f"avg P&L per trade (trades); this check {check.name}"
          + (f", previous {against.name}" if against else ""))
    print(f"{'':30s}{'IB only':>13}   {'full, IB win':>13}   {'full, all':>13}"
          f"   {'full, tape era':>13}   {'full, IB era':>13}")
    for s in strategies:
        print(f"{s:30s}{line(check, s)}")
        prev = line(against, s) if against else None
        if prev:
            print(f"{'  previous':30s}{prev}")


if __name__ == "__main__":
    main()
