#!/usr/bin/env python3
"""Run every strategy variant and tabulate their backtests side by side.

The price-target framework varies along five labeled axes — model, ranker,
portfolio, exit, and dataset. Each `strategies/*.toml` is one composition of
those labels, and every shipped variant differs from the `mean_reversion` base
on exactly **one** of them, so a difference in the table is attributable to that
axis. The `axis` column names the label that changed. Each row reports the
forecast headline (CRPS skill vs a random walk, directional hit-rate) from the
dense per-bar eval, and the trade guardrail (count, return, drawdown,
return/drawdown, profit factor, structure mix, exit mix).

Usage:
    python scripts/compare_strategies.py              # all strategies/*.toml
    python scripts/compare_strategies.py a.toml b.toml # just these

Requires a built binary (`cargo build`) and a capture snapshot on disk; each run
is a full backtest, so this takes a couple of minutes over the whole set.
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
from collections import Counter
from pathlib import Path

BIN = "target/debug/ib-spread-trader"
ROOT = Path(__file__).resolve().parent.parent
# The composition every other variant is a single-axis departure from.
BASE = "strategies/mean_reversion.toml"
AXES = ("model", "ranker", "portfolio", "exit", "dataset")


def labels(config: str) -> dict[str, str]:
    """The five block labels a strategy selects, as written."""
    text = (ROOT / config).read_text()
    found = {}
    for axis in AXES:
        match = re.search(rf'(?m)^{axis}\s*=\s*"([^"]+)"', text)
        if match:
            found[axis] = match.group(1)
    return found


def axis_of(config: str, base: dict[str, str]) -> str:
    """Which label this variant changed from the base — the comparison's point.

    A block given inline rather than by label reads as `inline`, because such a
    row is not a comparison against a reusable, validated block at all.
    """
    mine = labels(config)
    if mine == base:
        return "base"
    # More than one entry means the row is not an isolated single-axis comparison.
    return ", ".join(
        f"{axis}={mine.get(axis, 'inline')}"
        for axis in AXES
        if mine.get(axis) != base.get(axis)
    )


def structure(trade: dict) -> str:
    legs = re.findall(r"([+-])\d+([CP])", trade["description"])
    if len(legs) == 1:
        return "naked"
    return "credit" if trade["entry_debit"] < 0 else "debit"


def run(config: str, base: dict[str, str]) -> dict | None:
    out = subprocess.run(
        [BIN, "backtest", "--config", config, "--trades"],
        capture_output=True,
        text=True,
        cwd=ROOT,
    )
    if out.returncode != 0:
        sys.stderr.write(f"  {config}: {out.stderr.strip().splitlines()[-1:]}\n")
        return None
    d = json.loads(out.stdout)
    t = d["totals"]
    mix = Counter(structure(x) for x in d.get("trades", []))
    pooled = (d.get("model_eval") or {}).get("pooled") or {}
    # Top exit reasons, so the comparison shows *how* a variant closes.
    exits = Counter(t.get("exits_by_reason", {}))
    top_exits = ", ".join(f"{r}:{n}" for r, n in exits.most_common(3))
    return {
        "name": Path(config).stem,
        "axis": axis_of(config, base),
        "trades": t["trades"],
        "return_pct": t["return_pct"],
        "max_dd": t["max_drawdown"] * 100,
        "return_dd": t.get("return_over_drawdown") or 0.0,
        "pf": t.get("profit_factor") or 0.0,
        "crps_skill": pooled.get("crps_skill", float("nan")),
        "dir_hit": pooled.get("directional_hit_rate", float("nan")) * 100,
        "mix": "/".join(f"{n}{mix[k]}" for k, n in (("credit", "cr"), ("debit", "db"), ("naked", "nk")) if mix[k]),
        "exits": top_exits,
    }


def main() -> None:
    # Relative to the repo root, which `run` also uses as the working directory,
    # so the table is identical wherever the script is invoked from.
    configs = sys.argv[1:] or sorted(
        str(path.relative_to(ROOT)) for path in (ROOT / "strategies").glob("*.toml")
    )
    base = labels(BASE)
    rows = [r for c in configs if (r := run(str(c), base))]
    if not rows:
        sys.exit("no strategy produced a result")

    hdr = (
        f"{'strategy':>18}{'trades':>7}{'return%':>9}{'maxDD%':>8}{'ret/dd':>8}{'pf':>6}"
        f"{'crps':>9}{'dir%':>6}  {'structure':<15}{'axis':<26} exits"
    )
    print(hdr)
    print("-" * len(hdr))
    for r in rows:
        print(
            f"{r['name']:>18}{r['trades']:>7}{r['return_pct']:>9.1f}{r['max_dd']:>8.0f}"
            f"{r['return_dd']:>8.2f}{r['pf']:>6.2f}{r['crps_skill']:>+9.4f}{r['dir_hit']:>6.1f}  "
            f"{r['mix']:<15}{r['axis']:<26} {r['exits']}"
        )


if __name__ == "__main__":
    main()
