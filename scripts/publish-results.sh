#!/usr/bin/env bash
# Publish the frontier (out/) to the capture server, where the LAN
# visualizer reads it from ~/ib-results/current.
#
#   scripts/publish-results.sh
#
# Each run in out/ gets a bundle: ib-option-calc's `research.py bundle`
# cuts the research DB the run read (backtest.json's snapshot.path) down to
# its trades' chains and bars, so the server's visualizer can draw every trade
# without the full DB. A run made on a capture snapshot has no research DB to
# cut from, and is published without a bundle (its charts stay empty there).
# Then out/ goes to CT 106 as one tar, checked by SHA-256, unpacked into
# ~/ib-results/<stamp>/, and ~/ib-results/current is swapped to it in one
# rename. The last five publishes are kept.
set -euo pipefail
cd "$(dirname "$0")/.."

HOST="${IBCAPTURE_HOST:-root@192.168.1.50}"
CT="${IBCAPTURE_CT:-106}"
DEST="${RESULTS_DIR:-/home/capture/ib-results}"
KEEP="${KEEP_RESULTS:-5}"
CALC="${IB_OPTION_CALC:-}"
if [ -z "$CALC" ]; then
  for d in ../ibkr-options ../ib-option-calc; do
    [ -f "$d/scripts/research.py" ] && CALC="$d" && break
  done
fi
[ -n "$CALC" ] || { echo "can't find ib-option-calc's scripts/research.py; set IB_OPTION_CALC" >&2; exit 1; }
python="$(command -v python || command -v python3)"

shopt -s nullglob
runs=(out/*/backtest.json)
[ "${#runs[@]}" -gt 0 ] || { echo "nothing in out/ (scripts/run_frontier.sh)" >&2; exit 1; }
for report in "${runs[@]}"; do
  dir="$(dirname "$report")"
  db="$("$python" -I -c '
import json, pathlib, sys
s = json.load(open(sys.argv[1], encoding="utf-8")).get("snapshot") or {}
print(pathlib.Path(s["path"]).as_posix() if s.get("format") == "research" else "")' "$report")"
  if [ -z "$db" ]; then
    echo "$dir: not run on a research DB; published without a bundle" >&2
    rm -f "$dir/bundle.sqlite3"
    continue
  fi
  [ -f "$db" ] || { echo "$dir was run on $db, which is gone; rerun it" >&2; exit 1; }
  if [ ! -f "$dir/bundle.sqlite3" ] || [ "$report" -nt "$dir/bundle.sqlite3" ]; then
    rm -f "$dir/bundle.sqlite3"
    "$python" -I "$CALC/scripts/research.py" bundle --db "$db" --trades "$dir/trades.csv" \
      --out "$dir/bundle.sqlite3" >/dev/null
    echo "$dir: bundled ($(du -h "$dir/bundle.sqlite3" | cut -f1))" >&2
  fi
done

stamp="$(date +%Y-%m-%d-%H%M)"
"$python" -I - "$stamp" "$CALC" > out/manifest.json <<'MANIFEST'
import json, pathlib, subprocess, sys
from datetime import datetime, timezone
def head(repo):
    try:
        return subprocess.run(["git", "-C", repo, "rev-parse", "--short", "HEAD"], capture_output=True,
                              text=True, check=True).stdout.strip()
    except Exception:
        return None
calc = sys.argv[2]
print(json.dumps({
    "published": sys.argv[1],
    "published_at": datetime.now(timezone.utc).isoformat(timespec="seconds"),
    "commits": {"ib-spread-trader": head("."), "ib-option-calc": head(calc),
                "ib-option-viz": head(str(pathlib.Path(calc).parent / "ib-option-viz"))},
    "runs": sorted(p.parent.name for p in pathlib.Path("out").glob("*/backtest.json")),
}, indent=2))
MANIFEST

in_ct() { ssh -n -o BatchMode=yes "$HOST" "pct exec $CT -- $*"; }
in_ct mkdir -p "$DEST"
in_ct test ! -e "$DEST/$stamp" -a ! -e "$DEST/$stamp.tar.part" ||
  { echo "$DEST/$stamp already exists; try again in a minute" >&2; exit 1; }
sum_file="$(mktemp)"
tar --owner=0 --group=0 --numeric-owner -cf - out |
  tee >(sha256sum | cut -d' ' -f1 >"$sum_file") |
  ssh -o BatchMode=yes "$HOST" "pct exec $CT -- sh -c 'set -C; cat > $DEST/$stamp.tar.part'"
for _ in $(seq 1 600); do [ -s "$sum_file" ] && break; sleep 0.1; done
sent="$(cat "$sum_file")"
rm -f "$sum_file"
got="$(in_ct sha256sum "$DEST/$stamp.tar.part" | cut -d' ' -f1)"
[ -n "$sent" ] && [ "$sent" = "$got" ] ||
  { echo "SHA-256 mismatch (sent $sent, got $got); left $DEST/$stamp.tar.part" >&2; exit 1; }
ssh -o BatchMode=yes "$HOST" "pct exec $CT -- sh -s" <<REMOTE
set -e
cd $DEST
mkdir $stamp
tar -C $stamp --strip-components=1 -xf $stamp.tar.part
rm $stamp.tar.part
chown -R capture:capture $DEST
ln -sfn $stamp current.new
mv -T current.new current
chown -h capture:capture current
ls -1d 20[0-9][0-9]-* | sort | head -n -$KEEP | xargs -r rm -rf
ls -l current
REMOTE
echo "published $stamp: ${#runs[@]} runs to $DEST/current"
