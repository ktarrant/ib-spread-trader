#!/usr/bin/env python3
"""Validate a labeled price model and write its auditable registry record.

The model is evaluated once across the configured dataset and once per symbol.
The per-symbol runs are the leave-one-symbol-out robustness check for this fixed
(non-fitted) model: each held-out name must stand on its own, without its result
being diluted by the other eleven. Scorer sampling is disabled in the temporary
configs because it does not affect forecast metrics.

Usage:
    python scripts/validate_model.py strategies/mean_reversion.toml
    python scripts/validate_model.py strategies/driftless.toml --db snapshot.sqlite3
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
DEFAULT_BINARY = ROOT / "target/debug/ib-spread-trader"

MIN_CRPS_SKILL = 0.0
MIN_DIRECTIONAL_HIT_RATE = 0.5
MAX_PIT_CALIBRATION_ERROR = 0.15
MIN_LOSO_PASS_FRACTION = 0.66


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("config", type=Path)
    parser.add_argument("--db", type=Path, help="snapshot override")
    parser.add_argument("--binary", type=Path, default=DEFAULT_BINARY)
    parser.add_argument("--registry", type=Path, default=ROOT / "models")
    return parser.parse_args()


def validation_config(source: str, symbols: list[str]) -> str:
    replacement = "symbols = " + json.dumps(symbols)
    text, count = re.subn(r"(?m)^symbols\s*=\s*\[[^\n]*\]", replacement, source, count=1)
    if count != 1:
        raise ValueError("validation currently needs a top-level `symbols = [...]` array")
    if re.search(r"(?m)^scorer_sample_per_bar\s*=", text):
        text = re.sub(
            r"(?m)^scorer_sample_per_bar\s*=.*$",
            "scorer_sample_per_bar = 0",
            text,
        )
    return text


def scalar(source: str, key: str, default: str | None = None) -> str:
    match = re.search(rf'(?m)^{re.escape(key)}\s*=\s*"([^"]+)"\s*$', source)
    if match:
        return match.group(1)
    if default is not None:
        return default
    raise ValueError(f"validation config is missing `{key}`")


def backtest(binary: Path, config: Path, db: Path | None) -> dict:
    command = [str(binary)]
    if db is not None:
        command.extend(["--db", str(db)])
    command.extend(["backtest", "--config", str(config)])
    result = subprocess.run(command, cwd=ROOT, capture_output=True, text=True)
    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip()
        raise RuntimeError(f"backtest failed for {config}: {detail}")
    return json.loads(result.stdout)


def main() -> None:
    args = parse_args()
    config_path = args.config if args.config.is_absolute() else ROOT / args.config
    source = config_path.read_text()
    label = scalar(source, "name", config_path.stem)
    symbols_match = re.search(r"(?m)^symbols\s*=\s*(\[[^\n]*\])", source)
    symbols = json.loads(symbols_match.group(1)) if symbols_match else []
    expiry = scalar(source, "expiry")
    model_match = re.search(r"(?ms)^\[model\]\s*(.*?)(?=^\[|\Z)", source)
    horizon_match = (
        re.search(r"(?m)^horizon_days\s*=\s*([0-9.eE+-]+)", model_match.group(1))
        if model_match
        else None
    )
    horizon_days = float(horizon_match.group(1)) if horizon_match else 7.0
    if not symbols:
        raise ValueError("model validation requires non-empty symbols and an expiry")
    if not args.binary.exists():
        raise FileNotFoundError(f"build the binary first: {args.binary}")

    with tempfile.TemporaryDirectory(prefix="ib-model-validation-") as temp_name:
        temp = Path(temp_name)
        pooled_config = temp / "pooled.toml"
        pooled_config.write_text(validation_config(source, symbols))
        print(f"validating {label} across {len(symbols)} symbols", file=sys.stderr)
        pooled_report = backtest(args.binary, pooled_config, args.db)
        pooled = pooled_report["model_eval"]["pooled"]

        held_out_hits: list[float] = []
        for symbol in symbols:
            fold_config = temp / f"held-out-{symbol.lower()}.toml"
            fold_config.write_text(validation_config(source, [symbol]))
            print(f"  held out: {symbol}", file=sys.stderr)
            report = backtest(args.binary, fold_config, args.db)
            held_out_hits.append(report["model_eval"]["pooled"]["directional_hit_rate"])

    passing_symbols = sum(hit > MIN_DIRECTIONAL_HIT_RATE for hit in held_out_hits)
    required_symbols = math.ceil(MIN_LOSO_PASS_FRACTION * len(symbols))
    passed = (
        pooled["crps_skill"] > MIN_CRPS_SKILL
        and pooled["directional_hit_rate"] > MIN_DIRECTIONAL_HIT_RATE
        and pooled["pit_calibration_error"] < MAX_PIT_CALIBRATION_ERROR
        and passing_symbols >= required_symbols
    )
    record = {
        "label": label,
        "dataset": str(expiry),
        "horizon_days": horizon_days,
        "crps_skill": pooled["crps_skill"],
        "pit_error": pooled["pit_calibration_error"],
        "coverage_50": pooled["coverage_50"],
        "coverage_80": pooled["coverage_80"],
        "coverage_90": pooled["coverage_90"],
        "directional_hit_rate": pooled["directional_hit_rate"],
        "loso_min_dir_hit": min(held_out_hits),
        "loso_symbols_passing": passing_symbols,
        "loso_symbols_total": len(symbols),
        "passed": passed,
        "evaluated_at": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
    }

    registry = args.registry if args.registry.is_absolute() else ROOT / args.registry
    registry.mkdir(parents=True, exist_ok=True)
    destination = registry / f"{label}.json"
    rendered = json.dumps(record, indent=2) + "\n"
    destination.write_text(rendered)
    sys.stdout.write(rendered)


if __name__ == "__main__":
    main()
