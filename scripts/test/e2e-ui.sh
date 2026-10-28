#!/usr/bin/env bash
# Browser checks of the search UI (ui/e2e/search.cjs) and the admin pages (ui/e2e/admin.cjs)
# against a real sourcegull:
#   - a fresh database `sourcegull_e2e_<pid>_<random>` on the dev Postgres (scripts/dev-postgres.sh), or on the
#     server SOURCEGULL_E2E_DATABASE_URL points at (any database there; used for CREATE/DROP DATABASE)
#   - `sourcegull run --roles=all` on 127.0.0.1:7090 with a temporary data dir
#   - one repository, gitlab.example.com/acme/sourcegull: a mirror of this checkout, placed straight
#     into the repo node's data dir and marked cloned (no GitLab needed)
#
# Usage: scripts/test/e2e-ui.sh            (builds the UI with the WASM parser and the binary first)
#   CHECKS="search" runs only some of the checks (default: search admin codenav codegraph policies executor-secrets autoindex batch-changes batch-specs connections repos-admin updates watches; the others need search first)
#   CHECKS=gitea runs ui/e2e/gitea.cjs against the compose service `gitea` (GITEA_URL); scripts/dev
#   e2e starts it fresh first
# Needs: Playwright with Chromium resolvable by node (set NODE_PATH to its node_modules), e.g.
#   mkdir -p ~/.cache/sourcegull-e2e && cd ~/.cache/sourcegull-e2e && npm i playwright@1.49.1 && npx playwright install chromium
#   NODE_PATH=~/.cache/sourcegull-e2e/node_modules scripts/test/e2e-ui.sh
# In Docker, with nothing installed: scripts/dev e2e   (the `e2e` service of compose.yaml)
# SOURCEGULL_E2E_BIN runs that binary instead of building one (the UI is embedded in it);
# `SOURCEGULL_E2E_IMAGE=<image> scripts/dev e2e` sets it to the binary of a pulled image.
set -euo pipefail
cd "$(dirname "$0")/../.."
ROOT=$(pwd)
PORT=${PORT:-7090}
WORK=$(mktemp -d "${TMPDIR:-/tmp}/sourcegull-e2e.XXXXXX")
export OUT=${OUT:-$WORK/screenshots}
mkdir -p "$OUT"

if [ -n "${SOURCEGULL_E2E_DATABASE_URL:-}" ]; then
  PG=${SOURCEGULL_E2E_DATABASE_URL%%\?*}
elif [ -n "${SOURCEGULL_PGDATA:-}" ]; then
  PG=$(scripts/dev-postgres.sh "$SOURCEGULL_PGDATA")
else
  PG=$(scripts/dev-postgres.sh)
fi
PROFILE=${PROFILE:-debug}
case "$PROFILE" in
  debug) BIN=${CARGO_TARGET_DIR:-target}/debug/sourcegull; BUILD_ARGS=() ;;
  release) BIN=${CARGO_TARGET_DIR:-target}/release/sourcegull; BUILD_ARGS=(--release) ;;
  *) echo "PROFILE must be debug or release" >&2; exit 2 ;;
esac
if [ -n "${SOURCEGULL_E2E_BIN:-}" ]; then
  BIN=$SOURCEGULL_E2E_BIN
elif [ "${SKIP_BUILD:-}" = "" ]; then
  (cd ui && npm run wasm && npm run build)
  cargo build -p sourcegull "${BUILD_ARGS[@]}"
fi
# PIDs repeat between container runs, hence the random part.
DB="sourcegull_e2e_$$_$RANDOM"
psql "$PG" -qc "CREATE DATABASE $DB"
SERVER=
cleanup() {
  if [ -n "$SERVER" ]; then
    kill "$SERVER" 2>/dev/null || true
    wait "$SERVER" 2>/dev/null || true
  fi
  psql "$PG" -qc "DROP DATABASE IF EXISTS $DB WITH (FORCE)"
  echo "logs and screenshots: $WORK"
}
trap cleanup EXIT
export SOURCEGULL_DATABASE_URL="${PG%/*}/$DB"
psql_() { psql "$SOURCEGULL_DATABASE_URL" -tAqc "$1"; }
"$BIN" migrate up

DATA=$WORK/data
# An encryption key file, so that secrets (executor secrets) can be stored.
mkdir -p "$DATA"
( umask 077; printf 'k1:%s\n' "$(head -c 32 /dev/urandom | base64)" > "$DATA/keys" )
"$BIN" run --roles=all --data-dir "$DATA" \
  --http-addr 127.0.0.1:$PORT --internal-addr 127.0.0.1:$((PORT + 1)) \
  --repo-grpc-addr 127.0.0.1:$((PORT + 2)) --repo-node-addr http://127.0.0.1:$((PORT + 2)) \
  --repo-nodes http://127.0.0.1:$((PORT + 2)) > "$WORK/sourcegull.log" 2>&1 &
SERVER=$!
for _ in $(seq 1 120); do curl -sf "http://127.0.0.1:$PORT/healthz" >/dev/null && break; sleep 0.5; done

ID=$(psql_ "INSERT INTO repo (name, external_id, external_service_type, external_service_id, description)
  VALUES ('gitlab.example.com/acme/sourcegull', '1', 'gitlab', 'https://gitlab.example.com/', 'The Sourcegull sources')
  RETURNING id")
psql_ "INSERT INTO gitserver_repos (repo_id, clone_status, node) VALUES ($ID, 'cloned', 'http://127.0.0.1:$((PORT + 2))')
  ON CONFLICT (repo_id) DO UPDATE SET clone_status = 'cloned'"
SHARD=$(printf '%02x' $((ID % 256)))
mkdir -p "$DATA/repos/$SHARD"
git clone -q --mirror "$ROOT" "$DATA/repos/$SHARD/$ID.git"
git -C "$DATA/repos/$SHARD/$ID.git" symbolic-ref HEAD refs/heads/main
sleep 2

for check in ${CHECKS:-search admin codenav codegraph policies executor-secrets autoindex batch-changes batch-specs connections repos-admin updates analytics insights insights-create watches}; do
  echo "== $check"
  BASE="http://127.0.0.1:$PORT" node "ui/e2e/$check.cjs"
done
