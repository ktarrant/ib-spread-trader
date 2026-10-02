#!/usr/bin/env python3
"""Sweep price-model parameters and rank them by forecast quality.

The forecast eval does not depend on the ranker, the exit, or the portfolio: it
scores the model's distribution against the underlying's realized price a horizon
later, on every option bar. So a model search needs none of those, and it can turn
`scorer_sample_per_bar` off — which is the expensive per-bar work, and takes a run
from ~5 minutes to ~35 seconds.

    python3 scripts/sweep_model.py --horizon 14 \
        --set trend_weight=-0.5,0,0.5,1.0 --set vwap_reversion=0,0.5

Every `--set` multiplies into a grid. Output is one row per cell, sorted by CRPS
skill against the random-walk null — the gate a model has to clear before its
backtest P&L means anything.

**This is a search, and a search on one dataset overfits it.** The row at the top
of this table is a candidate, not a result; put it through
`scripts/validate_model.py` for the leave-one-symbol-out check before believing
it, and read the directional hit-rate and PIT beside the skill rather than
maximizing one number.
"""

from __future__ import annotations

import argparse
import itertools
import json
import subprocess
import sys
import tempfile
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_BINARY = ROOT / "target" / "release" / "ib-spread-trader"

# Held fixed across the grid: the vol blend and grid that the shipped models use.
BASE_MODEL = {
    "bars_per_year": 3276.0,
    "realized_vol_weight": 0.0,
    "fallback_vol": 0.30,
    "trend_weight": 0.0,
    "vwap_reversion": 0.0,
    "skew_weight": 0.0,
    "grid_sigma": 4.0,
    "grid_points": 101,
}
BASE_FEATURES = {
    "trend_fast": 10,
    "trend_slow": 30,
    "trend_average": "sma",
    "atr_period": 14,
    "realized_vol_period": 20,
    "skew_moneyness": 0.05,
}


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--horizon", type=float, default=14.0)
    parser.add_argument(
        "--set",
        action="append",
        default=[],
        metavar="FIELD=V1,V2",
        help="a model or feature field and the values to try; repeat to build a grid",
    )
    parser.add_argument("--dataset", default="2026-08-21", help="a label under datasets/")
    parser.add_argument("--db", type=Path)
    parser.add_argument("--binary", type=Path, default=DEFAULT_BINARY)
    parser.add_argument("--jobs", type=int, default=1, help="cells run in parallel")
    return parser.parse_args()


def render(model: dict, features: dict, horizon: float, dataset: str) -> str:
    def table(name: str, fields: dict) -> str:
        body = "\n".join(f"{k} = {json.dumps(v)}" for k, v in fields.items())
        return f"[{name}]\n{body}\n"

    # The entry blackout must cover the horizon, or validate() refuses the config.
    blackout = max(8, int(horizon + 0.999))
    return (
        'name = "sweep"\nmode = "ranked"\n'
        f'portfolio = "equal_weight_1"\nexit = "base"\ndataset = "{dataset}"\n\n'
        "[execution]\ntiming = \"next_bar\"\nearnings_blackout_days = 3\n"
        f"min_entry_days_before_expiry = {blackout}\n\n"
        + table("features", features)
        + "\n"
        + table("model", {**model, "horizon_days": horizon})
        + "\n[candidates]\nallow_debit = false\nallow_credit = true\n"
        "allow_naked_long = false\nmax_loss = 500.0\n"
        "\n[ranking]\nmetric = \"prob_profit\"\nmin_score = 0.02\nscorer_sample_per_bar = 0\n"
    )


def run(binary: Path, config: Path, db: Path | None) -> dict | None:
    command = [str(binary)]
    if db:
        command.extend(["--db", str(db)])
    command.extend(["backtest", "--config", str(config)])
    out = subprocess.run(command, capture_output=True, text=True, cwd=ROOT)
    if out.returncode != 0:
        sys.stderr.write(f"  failed: {out.stderr.strip().splitlines()[-1:]}\n")
        return None
    report = json.loads(out.stdout)
    model_eval = report.get("model_eval") or {}
    pooled = model_eval.get("pooled")
    if pooled:
        pooled["trades"] = report["totals"]["trades"]
        per_symbol = model_eval.get("per_symbol") or []
        pooled["symbols_positive"] = sum(s["crps_skill"] > 0 for s in per_symbol)
        pooled["symbols"] = len(per_symbol)
    return pooled


def parse_value(text: str) -> float | str:
    try:
        return float(text)
    except ValueError:
        return text.strip()


def fmt(value: float | str) -> str:
    return value if isinstance(value, str) else f"{value:g}"


def main() -> None:
    args = parse_args()
    axes: list[tuple[str, list[float]]] = []
    for spec in args.set:
        field, _, values = spec.partition("=")
        axes.append((field.strip(), [parse_value(v) for v in values.split(",")]))
    if not axes:
        sys.exit("nothing to sweep: pass at least one --set FIELD=V1,V2")

    cells = []
    with tempfile.TemporaryDirectory(prefix="ib-model-sweep-") as temp_name:
        temp = Path(temp_name)
        for cell in itertools.product(*(values for _, values in axes)):
            assignment = dict(zip((field for field, _ in axes), cell))
            model = dict(BASE_MODEL)
            features = dict(BASE_FEATURES)
            for field, value in assignment.items():
                if isinstance(value, float) and value.is_integer() and field in features:
                    value = int(value)
                (features if field in features else model)[field] = value
            label = " ".join(f"{k}={fmt(v)}" for k, v in assignment.items())
            config = temp / (label.replace(" ", "_").replace("=", "") + ".toml")
            config.write_text(render(model, features, args.horizon, args.dataset))
            cells.append((label, config))

        def one(cell: tuple[str, Path]) -> tuple[str, dict | None]:
            print(f"  {cell[0]}", file=sys.stderr)
            return cell[0], run(args.binary, cell[1], args.db)

        with ThreadPoolExecutor(max_workers=args.jobs) as pool:
            rows = [(label, p) for label, p in pool.map(one, cells) if p]

    if not rows:
        sys.exit("no cell produced a forecast")
    rows.sort(key=lambda r: -r[1]["crps_skill"])
    width = max(len(label) for label, _ in rows)
    header = (
        f"{'cell':<{width}}{'crps skill':>12}{'interval':>20}{'sym+':>7}{'dir%':>7}"
        f"{'cov 50/80/90':>16}{'PIT err':>9}{'PIT mean':>10}{'trades':>8}"
    )
    print(f"\n{args.dataset}, horizon {args.horizon:g}d, sorted by CRPS skill vs the random-walk null")
    print(header)
    print("-" * len(header))
    for label, p in rows:
        cov = f"{p['coverage_50']*100:.0f}/{p['coverage_80']*100:.0f}/{p['coverage_90']*100:.0f}"
        print(
            f"{label:<{width}}{p['crps_skill']:>+12.4f}"
            f"{'[%+.4f, %+.4f]' % tuple(p['crps_skill_interval']):>20}"
            f"{'%d/%d' % (p['symbols_positive'], p['symbols']):>7}"
            f"{p['directional_hit_rate']*100:>7.1f}"
            f"{cov:>16}{p['pit_calibration_error']:>9.3f}{p['pit_mean']:>10.3f}{p['trades']:>8}"
        )
    print("\nA search overfits the set it searches. Put the top row through "
          "scripts/validate_model.py before believing it.")


if __name__ == "__main__":
    main()
