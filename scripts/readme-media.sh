#!/usr/bin/env bash
# The README screenshots and search animation (docs/assets/readme-*), taken from a throwaway
# instance that holds only public open-source repositories:
#   - a fresh database `sourcegull_media_<random>` on the dev Postgres, dropped at the end
#   - `sourcegull run --roles=all` on 127.0.0.1:7090 with a temporary data dir
#   - the repositories in REPOS, mirrored from github.com straight into the repo node's data dir
#     (no connection or token), then indexed by the worker
#   - scripts/readme-media.cjs drives Chromium; frames and screenshots go to OUT
#
# Usage: scripts/dev media, which runs this in the e2e container (Playwright and Chromium) with
# the binary of a released image, then encodes the frames and screenshots as WebP into
# docs/assets/.
set -euo pipefail
cd "$(dirname "$0")/.."
PORT=${PORT:-7090}
WORK=$(mktemp -d "${TMPDIR:-/tmp}/sourcegull-media.XXXXXX")
export OUT=${OUT:-$WORK/out}
mkdir -p "$OUT"
# The query in readme-media.cjs finds results in these; code navigation is in tokio.
REPOS=${REPOS:-"tokio-rs/tokio tokio-rs/axum serde-rs/serde"}
BIN=${SOURCEGULL_MEDIA_BIN:?the sourcegull binary to run}
PG=${SOURCEGULL_E2E_DATABASE_URL%%\?*}

DB="sourcegull_media_$$_$RANDOM"
psql "$PG" -qc "CREATE DATABASE $DB"
SERVER=
cleanup() {
  if [ -n "$SERVER" ]; then
    kill "$SERVER" 2>/dev/null || true
    wait "$SERVER" 2>/dev/null || true
  fi
  psql "$PG" -qc "DROP DATABASE IF EXISTS $DB WITH (FORCE)"
  echo "logs: $WORK"
}
trap cleanup EXIT
export SOURCEGULL_DATABASE_URL="${PG%/*}/$DB"
psql_() { psql "$SOURCEGULL_DATABASE_URL" -tAqc "$1"; }
"$BIN" migrate up

DATA=$WORK/data
mkdir -p "$DATA"
( umask 077; printf 'k1:%s\n' "$(head -c 32 /dev/urandom | base64)" > "$DATA/keys" )
"$BIN" run --roles=all --data-dir "$DATA" \
  --http-addr 127.0.0.1:$PORT --internal-addr 127.0.0.1:$((PORT + 1)) \
  --repo-grpc-addr 127.0.0.1:$((PORT + 2)) --repo-node-addr http://127.0.0.1:$((PORT + 2)) \
  --repo-nodes http://127.0.0.1:$((PORT + 2)) > "$WORK/sourcegull.log" 2>&1 &
SERVER=$!
for _ in $(seq 1 120); do curl -sf "http://127.0.0.1:$PORT/healthz" >/dev/null && break; sleep 0.5; done

n=0
for repo in $REPOS; do
  n=$((n + 1))
  echo "== mirror $repo"
  ID=$(psql_ "INSERT INTO repo (name, external_id, external_service_type, external_service_id, description)
    VALUES ('github.com/$repo', '$n', 'github', 'https://github.com/', '') RETURNING id")
  SHARD=$(printf '%02x' $((ID % 256)))
  mkdir -p "$DATA/repos/$SHARD"
  # MIRRORS keeps the clones between runs; the instance gets a local copy.
  if [ -n "${MIRRORS:-}" ]; then
    [ -d "$MIRRORS/$repo.git" ] || git clone -q --mirror "https://github.com/$repo.git" "$MIRRORS/$repo.git"
    git clone -q --mirror "$MIRRORS/$repo.git" "$DATA/repos/$SHARD/$ID.git"
  else
    git clone -q --mirror "https://github.com/$repo.git" "$DATA/repos/$SHARD/$ID.git"
  fi
  psql_ "INSERT INTO gitserver_repos (repo_id, clone_status, node) VALUES ($ID, 'cloned', 'http://127.0.0.1:$((PORT + 2))')
    ON CONFLICT (repo_id) DO UPDATE SET clone_status = 'cloned'"
done

echo "== wait for the search index"
for _ in $(seq 1 360); do
  done_=$(psql_ "SELECT count(*) FROM search_index_repos WHERE index_status = 'indexed'")
  [ "$done_" -ge "$n" ] && break
  sleep 5
done
echo "  $done_ of $n repositories indexed"
# Without a connection every fetch fails and the status indicator shows it; the mirrors are
# current, so the next fetch is a month away.
psql_ "DELETE FROM repo_update_jobs"
psql_ "UPDATE gitserver_repos SET last_error = NULL, consecutive_failures = 0, last_fetched = now(),
  next_fetch_at = now() + interval '30 days'"

BASE="http://127.0.0.1:$PORT" node scripts/readme-media.cjs
# KEEP=1 leaves the instance running, to rerun the browser script against it.
[ -z "${KEEP:-}" ] || wait "$SERVER"
