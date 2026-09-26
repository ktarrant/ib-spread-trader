#!/usr/bin/env python3
"""Validate a ranker's score ordering and write its model-paired record.

The record lands at `rankers/<label>/<dataset>.json`, one per dataset.

Usage:
    python3 scripts/validate_ranker.py strategies/mean_reversion.toml --label ev
    python3 scripts/validate_ranker.py strategies/mean_reversion.toml --dataset 2026-10-16
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
import tempfile
from datetime import datetime, timezone
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_BINARY = ROOT / "target/release/ib-spread-trader"
MIN_DECILE_MONOTONICITY = 0.5


def scalar(source: str, key: str, default: str | None = None) -> str:
    match = re.search(rf'(?m)^{re.escape(key)}\s*=\s*"([^"]+)"\s*$', source)
    if match:
        return match.group(1)
    if default is not None:
        return default
    raise ValueError(f"ranker validation config is missing `{key}`")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("config", type=Path)
    parser.add_argument("--label")
    parser.add_argument("--model-label")
    parser.add_argument("--dataset", help="a dataset label, replacing the config's own")
    parser.add_argument("--db", type=Path)
    parser.add_argument("--binary", type=Path, default=DEFAULT_BINARY)
    parser.add_argument("--registry", type=Path, default=ROOT / "rankers")
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    config = args.config if args.config.is_absolute() else ROOT / args.config
    source = config.read_text()
    model_label = args.model_label or scalar(source, "model", scalar(source, "name", config.stem))
    label = args.label or scalar(source, "ranker", "ev")
    if args.dataset:
        source, count = re.subn(r'(?m)^dataset\s*=\s*"[^"]+"\s*$', f'dataset = "{args.dataset}"', source)
        if count != 1:
            raise ValueError("--dataset needs a config that names a dataset label")
    dataset_label = scalar(source, "dataset", "")
    dataset_source = (
        (ROOT / "datasets" / f"{dataset_label}.toml").read_text()
        if dataset_label
        else source
    )
    expiry = scalar(dataset_source, "expiry")
    command = [str(args.binary)]
    if args.db:
        command.extend(["--db", str(args.db)])
    print(f"validating ranker {label} with model {model_label} on {expiry}", file=sys.stderr)
    with tempfile.TemporaryDirectory(prefix="ib-ranker-validation-") as temp:
        run_config = Path(temp) / "validate.toml"
        run_config.write_text(source)
        command.extend(["backtest", "--config", str(run_config)])
        result = subprocess.run(command, cwd=ROOT, capture_output=True, text=True)
    if result.returncode != 0:
        detail = result.stderr.strip() or result.stdout.strip()
        raise RuntimeError(f"backtest failed: {detail}")
    scorer = json.loads(result.stdout).get("scorer_eval")
    if not scorer:
        raise ValueError("no scorer_eval; set ranking.scorer_sample_per_bar to at least 1")

    spearman = scorer["spearman"]
    monotonicity = scorer["decile_monotonicity"]
    record = {
        "label": label,
        "model_label": model_label,
        "dataset": dataset_label or expiry,
        "spearman": spearman,
        "decile_monotonicity": monotonicity,
        "passed": spearman > 0.0 and monotonicity >= MIN_DECILE_MONOTONICITY,
        "evaluated_at": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
    }
    registry = args.registry if args.registry.is_absolute() else ROOT / args.registry
    destination = registry / label / f"{dataset_label or expiry}.json"
    destination.parent.mkdir(parents=True, exist_ok=True)
    rendered = json.dumps(record, indent=2) + "\n"
    destination.write_text(rendered)
    sys.stdout.write(rendered)


if __name__ == "__main__":
    main()
