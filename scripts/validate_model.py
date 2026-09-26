#!/usr/bin/env python3
"""Validate a labeled price model on one dataset and write its registry record.

A model's forecast does not depend on the ranker, exits or portfolio, so this
renders its own minimal config — the model label, the dataset, scorer sampling
off — and needs no strategy file. One run gives the pooled eval and the
per-symbol evals; each symbol's forecasts are independent of the others', so the
per-symbol rows are exactly what a run per held-out symbol would give.

The record lands at `models/<label>/<dataset>.json`, one per dataset, so a second
dataset adds evidence rather than overwriting the first. `cargo test` recomputes
every record's verdict from its evidence (`src/model/validation.rs`), so these
thresholds must match the gate there.

Usage:
    python3 scripts/validate_model.py mean-reversion
    python3 scripts/validate_model.py mean-reversion --dataset 2026-10-16
"""

from __future__ import annotations

import argparse
import json
import math
import re
import subprocess
import sys
import tempfile
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_BINARY = ROOT / "target/release/ib-spread-trader"

MIN_CRPS_SKILL = 0.0  # applied to the interval's lower bound
MIN_DIRECTIONAL_HIT_RATE = 0.5  # only for a model that forecasts direction
MAX_PIT_CALIBRATION_ERROR = 0.15
MIN_SYMBOL_PASS_FRACTION = 0.66  # symbols with their own positive CRPS skill


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("label", help="a model label under models/")
    parser.add_argument("--dataset", default="2026-08-21", help="a dataset label under datasets/")
    parser.add_argument("--db", type=Path, help="snapshot override")
    parser.add_argument("--binary", type=Path, default=DEFAULT_BINARY)
    parser.add_argument("--registry", type=Path, default=ROOT / "models")
    return parser.parse_args()


def horizon_days(label: str) -> float:
    source = (ROOT / "models" / f"{label}.toml").read_text()
    match = re.search(r"(?m)^horizon_days\s*=\s*([0-9.eE+-]+)", source)
    return float(match.group(1)) if match else 7.0


def render(label: str, dataset: str, horizon: float) -> str:
    # The entry blackout must cover the horizon or `validate()` refuses the
    # config; it does not touch the forecast eval, which scores every bar.
    blackout = max(8, math.ceil(horizon))
    return (
        f'name = "validate-{label}"\nmodel = "{label}"\ndataset = "{dataset}"\n'
        'portfolio = "equal_weight_1"\nexit = "base"\n\n'
        f"[execution]\nmin_entry_days_before_expiry = {blackout}\n\n"
        "[ranking]\nscorer_sample_per_bar = 0\n"
    )


def backtest(binary: Path, config: Path, db: Path | None) -> dict:
    command = [str(binary)] + (["--db", str(db)] if db else []) + ["backtest", "--config", str(config)]
    result = subprocess.run(command, cwd=ROOT, capture_output=True, text=True)
    if result.returncode != 0:
        raise RuntimeError(f"backtest failed: {result.stderr.strip() or result.stdout.strip()}")
    return json.loads(result.stdout)


def main() -> None:
    args = parse_args()
    if not args.binary.exists():
        raise FileNotFoundError(f"build the binary first (cargo build --release): {args.binary}")
    horizon = horizon_days(args.label)

    with tempfile.TemporaryDirectory(prefix="ib-model-validation-") as temp:
        config = Path(temp) / "validate.toml"
        config.write_text(render(args.label, args.dataset, horizon))
        print(f"validating {args.label} on {args.dataset}", file=sys.stderr)
        model_eval = backtest(args.binary, config, args.db)["model_eval"]

    pooled = model_eval["pooled"]
    symbol_skills = [s["crps_skill"] for s in model_eval["per_symbol"]]
    with_skill = sum(skill > 0 for skill in symbol_skills)
    # A driftless model calls no direction, and owes the gate none.
    hit = pooled["directional_hit_rate"] if pooled["directional_forecasts"] else None
    passed = (
        pooled["crps_skill_interval"][0] > MIN_CRPS_SKILL
        and (hit is None or hit > MIN_DIRECTIONAL_HIT_RATE)
        and pooled["pit_calibration_error"] < MAX_PIT_CALIBRATION_ERROR
        and len(symbol_skills) > 0
        and with_skill >= math.ceil(MIN_SYMBOL_PASS_FRACTION * len(symbol_skills))
    )
    record = {
        "label": args.label,
        "dataset": args.dataset,
        "horizon_days": horizon,
        "crps_skill": pooled["crps_skill"],
        "crps_skill_interval": pooled["crps_skill_interval"],
        "weeks": pooled["weeks"],
        "pit_error": pooled["pit_calibration_error"],
        "coverage_50": pooled["coverage_50"],
        "coverage_80": pooled["coverage_80"],
        "coverage_90": pooled["coverage_90"],
        "directional_hit_rate": hit,
        "symbols_with_skill": with_skill,
        "symbols_total": len(symbol_skills),
        "passed": passed,
        "evaluated_at": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
    }

    registry = args.registry if args.registry.is_absolute() else ROOT / args.registry
    destination = registry / args.label / f"{args.dataset}.json"
    destination.parent.mkdir(parents=True, exist_ok=True)
    rendered = json.dumps(record, indent=2) + "\n"
    destination.write_text(rendered)
    sys.stdout.write(rendered)


if __name__ == "__main__":
    main()
