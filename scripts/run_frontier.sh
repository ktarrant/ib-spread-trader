#!/usr/bin/env bash
# Regenerate the frontier: the few runs the viz should offer, written to
# out/<strategy>_<dataset>/. Anything else in out/ is moved to out_archive/
# so the viz (which lists every out/**/backtest.json) shows only these.
# Snapshot: --db / $IB_SNAPSHOT, else the newest (see CLAUDE.md).
set -euo pipefail
cd "$(dirname "$0")/.."

# strategy  dataset — add a row per new dataset as captures land.
FRONTIER="
vol_timing     2026-08-21
vol_timing     2026-10-16
skewed_condor  etf-2026-08-21
skewed_condor  etf-2026-10-16
mean_reversion 2026-08-21
mean_reversion 2026-10-16
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
