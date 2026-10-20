#!/usr/bin/env bash
# Copy the capture server's headline-score cache beside the snapshots, where
# news_explore.py and the viz read it:
#
#   scripts/fetch_news_cache.sh      # -> ~/Documents/Options Database/news-llm.sqlite3
#
# and keeps a read-only dated copy in news/news-llm-YYYY-MM-DD.sqlite3, since
# the server's cache is rewritten as scoring continues. The subfolder is not
# optional: a dated *.sqlite3 beside the snapshots would win the default
# snapshot pick (latest date, then name — "news" sorts after "ib").
#
# ib-option-calc's `news-score` writes the cache beside the capture database
# after each daily headline sync, and is its one writer: this replaces the local
# copy. It is copied with SQLite's backup API inside the container (consistent
# even mid-scoring), then streamed straight here, as fetch-snapshot.sh does:
# nothing is staged on the Proxmox host, whose small disk holds every guest.
# IBCAPTURE_NEWS_CACHE is the path under the capture user's home, if the
# config moved the database.
set -euo pipefail

HOST="${IBCAPTURE_HOST:-root@192.168.1.50}"
CT="${IBCAPTURE_CT:-106}"
REMOTE="${IBCAPTURE_NEWS_CACHE:-.local/share/ib-option-calc/news-llm.sqlite3}"
DEST="${SNAPSHOT_DEST:-$HOME/Documents/Options Database}/news-llm.sqlite3"
# CT 106's /tmp is RAM; its /var/tmp is disk.
staged="/var/tmp/news-llm-$$.sqlite3"
trap 'rm -f "$DEST.part"; ssh -o BatchMode=yes "$HOST" "pct exec $CT -- rm -f $staged" >/dev/null 2>&1 || true' EXIT

# The Windows PC's Git Bash has no sqlite3 CLI: check with Python's module
# there (`python`; on Windows `python3` is the Microsoft Store stub).
quick_check() {
  if command -v sqlite3 >/dev/null; then
    sqlite3 -readonly "$1" 'PRAGMA quick_check;'
  else
    "$(command -v python || command -v python3)" -I -c '
import pathlib, sqlite3, sys
uri = pathlib.Path(sys.argv[1]).resolve().as_uri() + "?mode=ro"
print(sqlite3.connect(uri, uri=True).execute("PRAGMA quick_check").fetchone()[0])' "$1"
  fi
}

ssh -o BatchMode=yes "$HOST" "pct exec $CT -- su - capture -c 'python3 - $REMOTE $staged'" <<'BACKUP'
import sqlite3, sys
dst = sqlite3.connect(sys.argv[2])
sqlite3.connect(sys.argv[1]).backup(dst)
dst.close()
BACKUP
ssh -o BatchMode=yes "$HOST" "pct exec $CT -- cat $staged" >"$DEST.part"
[ "$(quick_check "$DEST.part")" = ok ] || { echo "quick_check failed; rerun" >&2; exit 1; }
mv "$DEST.part" "$DEST"
snap="$(dirname "$DEST")/news/news-llm-$(date +%F).sqlite3"
mkdir -p "$(dirname "$snap")"
cp -f "$DEST" "$snap" && chmod 444 "$snap"
echo "$DEST"
echo "$snap"
