#!/usr/bin/env bash
# Copy the capture server's headline-score cache beside the snapshots, where
# news_explore.py and the viz read it:
#
#   scripts/fetch_news_cache.sh      # -> ~/Documents/Options Database/news-llm.sqlite3
#
# ib-option-calc's `news-score` writes the cache beside the capture database
# after each daily headline sync, and is its one writer: this replaces the local
# copy. It travels container -> Proxmox host -> here, as fetch-snapshot.sh does.
# IBCAPTURE_NEWS_CACHE is the path under the capture user's home, if the
# config moved the database.
set -euo pipefail

HOST="${IBCAPTURE_HOST:-root@192.168.1.50}"
CT="${IBCAPTURE_CT:-106}"
REMOTE="${IBCAPTURE_NEWS_CACHE:-.local/share/ib-option-calc/news-llm.sqlite3}"
DEST="${SNAPSHOT_DEST:-$HOME/Documents/Options Database}/news-llm.sqlite3"
staged=/var/tmp/news-llm.sqlite3
trap 'rm -f "$DEST.part"; ssh -o BatchMode=yes "$HOST" "rm -f $staged; pct exec $CT -- rm -f /tmp/news-llm.sqlite3" >/dev/null 2>&1 || true' EXIT

# ponytail: a plain copy, so it can tear if it lands mid-write. Scoring runs
# once a day after the 18:00 ET sync and writes small transactions; the
# quick_check below refuses a torn copy, so rerun if it does.
ssh -o BatchMode=yes "$HOST" bash -s <<EOF
set -euo pipefail
pct exec $CT -- su - capture -c "cp $REMOTE /tmp/news-llm.sqlite3"
pct pull $CT /tmp/news-llm.sqlite3 $staged
EOF
scp -q "$HOST:$staged" "$DEST.part"
[ "$(sqlite3 -readonly "$DEST.part" 'PRAGMA quick_check;')" = ok ] || { echo "quick_check failed; rerun" >&2; exit 1; }
mv "$DEST.part" "$DEST"
echo "$DEST"
