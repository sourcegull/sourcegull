#!/usr/bin/env bash
# The opt-in repo node scale-out case of scripts/test/helm-ct.sh (SOURCEGULL_CT_REBALANCE=1). Runs
# in the chart-testing image (bash, curl, kubectl, helm) against the kind cluster: installs the chart with 3 repo replicas, seeds repositories (rows only, no code host:
# each move is a reassignment plus a queued clone), upgrades to 4 replicas, and polls the
# `repoRebalance` GraphQL query until every planned move has flipped or finished.
#
#   scripts/test/helm-ct-rebalance.sh CHART VALUES
#
# Environment: REBALANCE_TIMEOUT (seconds to wait for the moves, default 600), REBALANCE_REPOS
# (repositories to seed, default 24; about a quarter of them move to the new node).
# GraphQL queries are single-quoted.
# shellcheck disable=SC2016
set -euo pipefail

CHART=${1:?chart}
VALUES=${2:?values file}
TIMEOUT=${REBALANCE_TIMEOUT:-600}
REPOS=${REBALANCE_REPOS:-24}
NS=sourcegull-rebalance-$RANDOM
BASE=http://127.0.0.1:17080
WORK=$(mktemp -d)
PF=

fail() { echo "FAIL: $*" >&2; exit 1; }
cleanup() {
  if [ -n "$PF" ]; then kill "$PF" 2>/dev/null || true; fi
  helm uninstall sourcegull -n "$NS" --wait >/dev/null 2>&1 || true
  kubectl delete namespace "$NS" --wait=false >/dev/null 2>&1 || true
  rm -rf "$WORK"
}
trap cleanup EXIT

psql() {
  kubectl -n "$NS" exec -i sourcegull-postgresql-0 -c postgresql -- \
    psql -qtAU sourcegull -d sourcegull -v ON_ERROR_STOP=1 "$@"
}

# The integer value of "key" in the JSON in $WORK/gql.
jint() { grep -o "\"$1\":[0-9]*" "$WORK/gql" | head -n 1 | cut -d: -f2; }

echo "== rebalance: install with 3 repo replicas ($NS)"
start=$SECONDS
helm install sourcegull "$CHART" -n "$NS" --create-namespace -f "$VALUES" \
  --set repo.replicaCount=3 --wait --timeout 900s
echo "installed in $((SECONDS - start)) s"

echo "== rebalance: seed $REPOS repositories"
psql -c "WITH r AS (
    INSERT INTO repo (name, external_id, external_service_type, external_service_id)
    SELECT 'gitlab.ct.invalid/rebalance/r' || i, 'ct-' || i, 'gitlab', 'https://gitlab.ct.invalid/'
    FROM generate_series(1, $REPOS) AS i RETURNING id)
  INSERT INTO gitserver_repos (repo_id) SELECT id FROM r"
for _ in $(seq 1 120); do
  [ "$(psql -c "SELECT count(*) FROM repo WHERE shard IS NULL")" = 0 ] && break
  sleep 2
done
[ "$(psql -c "SELECT count(*) FROM repo WHERE shard IS NULL")" = 0 ] ||
  fail "repositories still unassigned after 240 s"
psql -c "SELECT shard, count(*) FROM repo GROUP BY shard ORDER BY shard"

echo "== rebalance: upgrade to 4 repo replicas"
start=$SECONDS
helm upgrade sourcegull "$CHART" -n "$NS" --reuse-values --set repo.replicaCount=4 --wait --timeout 900s
echo "upgraded in $((SECONDS - start)) s"

kubectl -n "$NS" port-forward svc/sourcegull-web 17080:7080 >/dev/null 2>&1 &
PF=$!
for _ in $(seq 1 60); do
  [ "$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 "$BASE/readyz")" = 200 ] && break
  sleep 2
done
password=$(head -c 18 /dev/urandom | od -An -tx1 | tr -d ' \n')
code=$(curl -sS -o "$WORK/body" -D "$WORK/headers" -w '%{http_code}' --max-time 60 \
  -H 'Content-Type: application/json' -H 'X-Requested-With: Sourcegull' \
  --data "{\"username\":\"ct-admin\",\"password\":\"$password\",\"email\":\"ct-admin@example.com\"}" \
  "$BASE/-/sign-up")
[ "$code" = 200 ] || fail "sign-up returned $code: $(cat "$WORK/body")"
COOKIE=$(sed -n 's/^[Ss]et-[Cc]ookie: *\(sourcegull_session=[^;]*\).*/\1/p' "$WORK/headers" | tr -d '\r' | head -n 1)
[ -n "$COOKIE" ] || fail "sign-up: no session cookie"

query='query { repoRebalance { waitingFor queued copying flipped failed done cancelled } }'
echo "== rebalance: waiting for the moves (timeout ${TIMEOUT} s)"
deadline=$((SECONDS + TIMEOUT))
while :; do
  code=$(curl -sS -o "$WORK/gql" -w '%{http_code}' --max-time 30 -H "Cookie: $COOKIE" \
    -H 'X-Requested-With: Sourcegull' -H 'Content-Type: application/json' \
    --data "{\"query\":\"$query\"}" "$BASE/.api/graphql" || echo 000)
  if [ "$code" = 200 ] && ! grep -q '"errors"' "$WORK/gql"; then
    queued=$(jint queued) copying=$(jint copying) flipped=$(jint flipped)
    failed=$(jint failed) finished=$(jint "done")
    settled=no
    grep -q '"waitingFor":\[\]' "$WORK/gql" && settled=yes
    echo "settled=$settled queued=$queued copying=$copying flipped=$flipped done=$finished failed=$failed"
    [ "$failed" = 0 ] || fail "$failed repo moves failed: $(cat "$WORK/gql")"
    if [ "$settled" = yes ] && [ "$queued" = 0 ] && [ "$copying" = 0 ] &&
      [ $((flipped + finished)) -gt 0 ]; then
      break
    fi
  else
    echo "repoRebalance returned $code: $(head -c 500 "$WORK/gql")"
  fi
  [ "$SECONDS" -lt "$deadline" ] ||
    fail "repo moves not done after ${TIMEOUT} s; last status: $(cat "$WORK/gql")"
  sleep 5
done
psql -c "SELECT shard, count(*) FROM repo GROUP BY shard ORDER BY shard"
[ "$(psql -c "SELECT count(*) FROM repo WHERE shard LIKE '%sourcegull-repo-3.%'")" -gt 0 ] ||
  fail "no repository was assigned to the new node sourcegull-repo-3"
echo "rebalance OK"
