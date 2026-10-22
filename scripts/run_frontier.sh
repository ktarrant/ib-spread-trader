#!/usr/bin/env bash
# Regenerate the frontier: the few runs the viz should offer, written to
# out/<strategy>_<dataset>/. Anything else in out/ is moved to out_archive/
# so the viz (which lists every out/**/backtest.json) shows only these.
# Snapshot: --db / $IB_SNAPSHOT, else the newest (see CLAUDE.md).
#
#   scripts/run_frontier.sh [--jobs N] [--only STRATEGY[,STRATEGY...]] [backtest args]
#
# Runs go in parallel, N at a time (default 4): each is one core, and a
# history run peaks at ~5 GB. --only reruns those strategies' rows and leaves
# the rest of out/ alone. For deciding whether a change helps, use
# scripts/quick_check.sh first; this is for publishing.
set -euo pipefail
cd "$(dirname "$0")/.."

jobs=4
only=""
while [ $# -gt 0 ]; do
  case "$1" in
    --jobs) jobs=$2; shift 2 ;;
    --only) only=",$2,"; shift 2 ;;
    *) break ;;
  esac
done

# strategy  dataset — `all` / `etf-all` span every monthly chain, so a new
# capture lands by adding its expiry to those datasets, not a row here.
# `history` / `etf-history` are every monthly a research DB holds, back to
# 2023-11 from UW's tape (scripts/history_datasets.py); run them with --db on
# one. On a capture snapshot they have only the captured chains.
# Longest first, so the short runs fill in behind the long ones (minutes on a
# research DB, serially, before GridPricer, 2026-10-21: 34, 20, 20, 3, 10, 4,
# 3, 1; vol_timing_history is 14 after it).
FRONTIER="
vol_timing     history
mean_reversion history
mean_reversion_prevailing history
skewed_condor  etf-history
vol_timing     all
mean_reversion_prevailing all
mean_reversion all
skewed_condor  etf-all
"

cargo build -q --release
tmp=$(mktemp -d)
keep=()
running=0
failed=0
start=$SECONDS
while read -r strategy dataset; do
  [ -z "$strategy" ] && continue
  run="${strategy}_${dataset}"
  keep+=("$run")
  [ -n "$only" ] && [[ "$only" != *",$strategy,"* ]] && continue
  # Labels resolve under the source tree, so the config can live anywhere.
  sed "s/^dataset *=.*/dataset = \"$dataset\"/" "strategies/$strategy.toml" > "$tmp/$run.toml"
  mkdir -p "out/$run"
  echo "running $run" >&2
  (
    t0=$SECONDS
    if ./target/release/ib-spread-trader backtest --config "$tmp/$run.toml" --out "out/$run" "$@" \
        > /dev/null 2> "$tmp/$run.log"; then
      echo "done $run in $(( (SECONDS - t0) / 60 )) min" >&2
    else
      echo "FAILED $run:" >&2; tail -5 "$tmp/$run.log" >&2; exit 1
    fi
  ) &
  running=$((running + 1))
  if [ "$running" -ge "$jobs" ]; then
    wait -n || failed=1
    running=$((running - 1))
  fi
done <<< "$FRONTIER"
while [ "$running" -gt 0 ]; do
  wait -n || failed=1
  running=$((running - 1))
done
echo "frontier in $(( (SECONDS - start) / 60 )) min" >&2
[ "$failed" -eq 0 ] || { echo "some runs failed; out/ left as is" >&2; exit 1; }

[ -n "$only" ] && exit 0
mkdir -p out_archive
for dir in out/*/; do
  name=$(basename "$dir")
  [[ " ${keep[*]} " == *" $name "* ]] || { rm -rf "out_archive/$name"; mv "$dir" out_archive/; }
done
