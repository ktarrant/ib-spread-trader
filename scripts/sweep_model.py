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
    pooled = (report.get("model_eval") or {}).get("pooled")
    if pooled:
        pooled["trades"] = report["totals"]["trades"]
    return pooled


def main() -> None:
    args = parse_args()
    axes: list[tuple[str, list[float]]] = []
    for spec in args.set:
        field, _, values = spec.partition("=")
        axes.append((field.strip(), [float(v) for v in values.split(",")]))
    if not axes:
        sys.exit("nothing to sweep: pass at least one --set FIELD=V1,V2")

    rows = []
    with tempfile.TemporaryDirectory(prefix="ib-model-sweep-") as temp_name:
        temp = Path(temp_name)
        for cell in itertools.product(*(values for _, values in axes)):
            assignment = dict(zip((field for field, _ in axes), cell))
            model = dict(BASE_MODEL)
            features = dict(BASE_FEATURES)
            for field, value in assignment.items():
                if field in features:
                    features[field] = int(value) if float(value).is_integer() else value
                else:
                    model[field] = value
            label = " ".join(f"{k}={v:g}" for k, v in assignment.items())
            config = temp / (label.replace(" ", "_").replace("=", "") + ".toml")
            config.write_text(render(model, features, args.horizon, args.dataset))
            print(f"  {label}", file=sys.stderr)
            pooled = run(args.binary, config, args.db)
            if pooled:
                rows.append((label, pooled))

    if not rows:
        sys.exit("no cell produced a forecast")
    rows.sort(key=lambda r: -r[1]["crps_skill"])
    width = max(len(label) for label, _ in rows)
    header = (
        f"{'cell':<{width}}{'crps skill':>12}{'dir%':>7}{'cov 50/80/90':>16}"
        f"{'PIT err':>9}{'PIT mean':>10}{'trades':>8}"
    )
    print(f"\n{args.dataset}, horizon {args.horizon:g}d, sorted by CRPS skill vs the random-walk null")
    print(header)
    print("-" * len(header))
    for label, p in rows:
        cov = f"{p['coverage_50']*100:.0f}/{p['coverage_80']*100:.0f}/{p['coverage_90']*100:.0f}"
        print(
            f"{label:<{width}}{p['crps_skill']:>+12.4f}{p['directional_hit_rate']*100:>7.1f}"
            f"{cov:>16}{p['pit_calibration_error']:>9.3f}{p['pit_mean']:>10.3f}{p['trades']:>8}"
        )
    print("\nA search overfits the set it searches. Put the top row through "
          "scripts/validate_model.py before believing it.")


if __name__ == "__main__":
    main()
