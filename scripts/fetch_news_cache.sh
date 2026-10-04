#!/usr/bin/env bash
# Copy the capture server's news score cache beside the snapshots, where
# news_explore.py and the viz read it:
#
#   scripts/fetch_news_cache.sh      # -> ~/Documents/Options Database/news-llm.sqlite3
#
# The server scores headlines daily (deploy/systemd/ib-spread-trader-news-score.service)
# and is the cache's one writer: this replaces the local copy. The copy is taken
# with SQLite's backup API, so it is consistent even mid-run, and travels
# container -> Proxmox host -> here, the way ib-option-calc's fetch-snapshot.sh does.
set -euo pipefail

HOST="${IBCAPTURE_HOST:-root@192.168.1.50}"
CT="${IBCAPTURE_CT:-106}"
DEST="${SNAPSHOT_DEST:-$HOME/Documents/Options Database}/news-llm.sqlite3"
staged=/var/tmp/news-llm.sqlite3
trap 'rm -f "$DEST.part"; ssh -o BatchMode=yes "$HOST" "rm -f $staged; pct exec $CT -- rm -f /tmp/news-llm.sqlite3" >/dev/null 2>&1 || true' EXIT

ssh -o BatchMode=yes "$HOST" bash -s <<EOF
set -euo pipefail
pct exec $CT -- su - capture -c "python3 -c 'import os, sqlite3; s = sqlite3.connect(\"file:\" + os.path.expanduser(\"~/news-llm.sqlite3\") + \"?mode=ro\", uri=True); d = sqlite3.connect(\"/tmp/news-llm.sqlite3\"); s.backup(d); d.close()'"
pct pull $CT /tmp/news-llm.sqlite3 $staged
EOF
scp -q "$HOST:$staged" "$DEST.part"
[ "$(sqlite3 -readonly "$DEST.part" 'PRAGMA quick_check;')" = ok ] || { echo "quick_check failed" >&2; exit 1; }
mv "$DEST.part" "$DEST"
echo "$DEST"
