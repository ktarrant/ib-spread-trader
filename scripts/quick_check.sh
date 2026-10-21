#!/usr/bin/env bash
# The quick check: does a change help, measured against IB? ~6 minutes.
#
#   scripts/quick_check.sh [--research DB] [--capture DB] [--label NAME] [STRATEGY[:DATASET] ...]
#
# Runs each strategy (default: the frontier's four) on all.toml's monthlies
# (etf-all for the condor), twice, all in parallel: on the IB capture snapshot
# alone, and on the full research DB (IB plus UW's tape). Then
# scripts/seam_compare.py prints the average trade inside the IB run's own
# entry window for both, beside the previous check's. The capture-only run is
# the reference; a change is better when the full run moves toward it without
# losing its trades. See CLAUDE.md, "Commands".
#
# Snapshots: --capture / $IB_SNAPSHOT, else the backtester's default (the
# newest in ~/Documents/Options Database); --research / $IB_RESEARCH, else the
# newest D:/ib-research/research-*.sqlite3. Output: out_check/<stamp>[-label]/,
# outside out/ so the viz never lists it.
set -euo pipefail
cd "$(dirname "$0")/.."

research=${IB_RESEARCH:-}
capture=${IB_SNAPSHOT:-}
label=""
while [ $# -gt 0 ]; do
  case "$1" in
    --research) research=$2; shift 2 ;;
    --capture) capture=$2; shift 2 ;;
    --label) label="-$2"; shift 2 ;;
    *) break ;;
  esac
done
[ $# -gt 0 ] || set -- vol_timing skewed_condor mean_reversion mean_reversion_prevailing
if [ -z "$research" ]; then
  research=$(ls D:/ib-research/research-*.sqlite3 2>/dev/null | sort | tail -1)
  [ -n "$research" ] || { echo "no research DB: pass --research" >&2; exit 1; }
fi

cargo build -q --release
dir="out_check/$(date +%Y-%m-%d-%H%M%S)$label"
mkdir -p "$dir"
echo "research: $research" >&2
echo "capture:  ${capture:-(default snapshot)}" >&2
start=$SECONDS
failed=0
pids=()
for arg in "$@"; do
  strategy=${arg%%:*}
  dataset=${arg#*:}
  if [ "$dataset" = "$arg" ]; then
    case "$strategy" in *condor*) dataset=etf-all ;; *) dataset=all ;; esac
  fi
  [ -f "strategies/$strategy.toml" ] || { echo "no strategies/$strategy.toml" >&2; exit 1; }
  sed "s/^dataset *=.*/dataset = \"$dataset\"/" "strategies/$strategy.toml" > "$dir/$strategy.toml"
  for side in ib full; do
    db=()
    if [ "$side" = full ]; then db=(--db "$research"); elif [ -n "$capture" ]; then db=(--db "$capture"); fi
    (
      ./target/release/ib-spread-trader "${db[@]}" backtest --config "$dir/$strategy.toml" \
        --out "$dir/$strategy-$side" > /dev/null 2> "$dir/$strategy-$side.log" \
        || { echo "FAILED $strategy on $side:" >&2; tail -5 "$dir/$strategy-$side.log" >&2; exit 1; }
    ) &
    pids+=($!)
  done
done
for pid in "${pids[@]}"; do wait "$pid" || failed=1; done
echo "ran in $(( (SECONDS - start) / 60 )) min: $dir" >&2
[ "$failed" -eq 0 ] || exit 1
python -I -X utf8 scripts/seam_compare.py "$dir"
