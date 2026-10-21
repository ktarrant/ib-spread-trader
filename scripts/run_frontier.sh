#!/usr/bin/env bash
# Regenerate the frontier: the few runs the viz should offer, written to
# out/<strategy>_<dataset>/. Anything else in out/ is moved to out_archive/
# so the viz (which lists every out/**/backtest.json) shows only these.
# Snapshot: --db / $IB_SNAPSHOT, else the newest (see CLAUDE.md).
set -euo pipefail
cd "$(dirname "$0")/.."

# strategy  dataset — `all` / `etf-all` span every monthly chain, so a new
# capture lands by adding its expiry to those datasets, not a row here.
# `history` / `etf-history` are every monthly a research DB holds, back to
# 2023-11 from UW's tape (scripts/history_datasets.py); run them with --db on
# one. On a capture snapshot they have only the captured chains.
FRONTIER="
vol_timing     all
skewed_condor  etf-all
mean_reversion all
mean_reversion_prevailing all
vol_timing     history
skewed_condor  etf-history
mean_reversion history
mean_reversion_prevailing history
"

cargo build -q --release
tmp=$(mktemp -d)
keep=()
while read -r strategy dataset; do
  [ -z "$strategy" ] && continue
  run="${strategy}_${dataset}"
  keep+=("$run")
  # Labels resolve under the source tree, so the config can live anywhere.
  sed "s/^dataset *=.*/dataset = \"$dataset\"/" "strategies/$strategy.toml" > "$tmp/$run.toml"
  mkdir -p "out/$run"
  echo "running $run" >&2
  ./target/release/ib-spread-trader backtest --config "$tmp/$run.toml" --out "out/$run" "$@" > /dev/null
done <<< "$FRONTIER"

mkdir -p out_archive
for dir in out/*/; do
  name=$(basename "$dir")
  [[ " ${keep[*]} " == *" $name "* ]] || { rm -rf "out_archive/$name"; mv "$dir" out_archive/; }
done
