#!/usr/bin/env bash
# GraphQL variables ($u) are in single-quoted queries.
# shellcheck disable=SC2016
# Data written before an upgrade and checked after it (used by scripts/test/upgrade.sh). Needs only bash and curl (it runs in the chart-testing image).
#
#   smoke.sh seed   BASE STATE   on version N-2: signs up the site admin, creates an access token,
#                                a saved search, a global executor secret and a GitLab connection
#                                (both encrypted at rest), runs a search; writes the credentials
#                                to STATE (shell variables)
#   smoke.sh verify BASE STATE   on version N: signs in with the old password, uses the old token,
#                                finds the saved search, the executor secret and the connection,
#                                reads the code monitor webhook URL of legacy-monitor.sql,
#                                runs a search, and waits until every out-of-band migration has
#                                completed without errors
#
# BASE is the web URL (http://web:7080). Only the API of the oldest version tested may be used
# when seeding.
set -euo pipefail

MODE=${1:?seed or verify}
BASE=${2:?base URL}
STATE=${3:?state file}
OOB_TIMEOUT=${OOB_TIMEOUT:-600}

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT
COOKIE=

fail() { echo "FAIL: $*" >&2; exit 1; }

# The first string value of "key" in the JSON on stdin.
jstr() { grep -o "\"$1\":\"[^\"]*\"" | head -n 1 | sed "s/^\"$1\":\"//; s/\"\$//"; }

# post PATH JSON: POST with the CSRF header; prints the status code, body in $WORK/body.
post() {
  curl -sS -o "$WORK/body" -D "$WORK/headers" -w '%{http_code}' --max-time 60 \
    -H 'Content-Type: application/json' -H 'X-Requested-With: Sourcegull' --data "$2" "$BASE$1"
}

# gql QUERY [VARIABLES] with the session cookie (or AUTH, an Authorization header value): the
# response body on stdout; fails on HTTP or GraphQL errors (a top-level "errors", not a field).
gql() {
  local query=${1//\"/\\\"} vars=${2:-'{}'} code
  local auth=(-H "Cookie: $COOKIE")
  [ -n "${AUTH:-}" ] && auth=(-H "Authorization: $AUTH")
  code=$(curl -sS -o "$WORK/gql" -w '%{http_code}' --max-time 60 "${auth[@]}" \
    -H 'X-Requested-With: Sourcegull' -H 'Content-Type: application/json' \
    --data "{\"query\":\"$query\",\"variables\":$vars}" "$BASE/.api/graphql")
  if [ "$code" != 200 ] || grep -q '^{"errors"\|^{"data":null,"errors"\|},"errors":\[' "$WORK/gql"; then
    fail "GraphQL returned $code: $(head -c 2000 "$WORK/gql")"
  fi
  cat "$WORK/gql"
}

wait_ready() {
  for _ in $(seq 1 120); do
    if [ "$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 "$BASE/readyz")" = 200 ]; then
      return 0
    fi
    sleep 2
  done
  fail "$BASE/readyz not ready after 240 s"
}

sign_in() {
  local path=$1 body=$2 code
  code=$(post "$path" "$body")
  [ "$code" = 200 ] || fail "$path returned $code: $(cat "$WORK/body")"
  COOKIE=$(sed -n 's/^[Ss]et-[Cc]ookie: *\(sourcegull_session=[^;]*\).*/\1/p' "$WORK/headers" | tr -d '\r' | head -n 1)
  [ -n "$COOKIE" ] || fail "$path: no session cookie"
}

search() {
  local q=$1 code
  code=$(curl -sS -o "$WORK/search" -w '%{http_code}' --max-time 60 -H "Cookie: $COOKIE" \
    -H 'Accept: text/event-stream' -G --data-urlencode "q=$q" --data-urlencode 't=keyword' \
    "$BASE/.api/search/stream")
  [ "$code" = 200 ] && grep -q '^event: done' "$WORK/search" ||
    fail "search returned $code without a done event: $(head -c 1000 "$WORK/search")"
  ! grep -q '^event: error' "$WORK/search" || fail "search streamed an error: $(head -c 1000 "$WORK/search")"
  echo "OK: search '$q'"
}

SAVED_QUERY='upgrade-check count:5 patternType:keyword'
SECRET_KEY=UPGRADE_CHECK_SECRET
CONNECTION=upgrade-check-gitlab

seed() {
  wait_ready
  local user=upgrade-admin password
  password=$(head -c 18 /dev/urandom | od -An -tx1 | tr -d ' \n')
  sign_in /-/sign-up "{\"username\":\"$user\",\"password\":\"$password\",\"email\":\"$user@example.com\"}"
  echo "OK: signed up $user"
  local me id admin
  me=$(gql 'query { currentUser { id username siteAdmin } }')
  id=$(echo "$me" | jstr id)
  admin=$(echo "$me" | grep -o '"siteAdmin":[a-z]*')
  [ "$admin" = '"siteAdmin":true' ] || fail "the first user is not site admin: $me"
  local token
  token=$(gql 'mutation($u: ID!) { createAccessToken(user: $u, scopes: ["user:all"], note: "upgrade check") { id token } }' \
    "{\"u\":\"$id\"}" | jstr token)
  [ -n "$token" ] || fail "no access token"
  echo "OK: access token"
  gql 'mutation($o: ID!, $q: String!) { createSavedSearch(input: {owner: $o, description: "upgrade check", query: $q, draft: false, visibility: SECRET}) { id } }' \
    "{\"o\":\"$id\",\"q\":\"$SAVED_QUERY\"}" >/dev/null
  echo "OK: saved search"
  gql "mutation { createExecutorSecret(scope: BATCHES, key: \"$SECRET_KEY\", value: \"s3cr3t-$password\") { id } }" >/dev/null
  echo "OK: executor secret"
  gql "mutation(\$c: String!) { addExternalService(input: {kind: GITLAB, displayName: \"$CONNECTION\", config: \$c}) { id } }" \
    '{"c":"{\"url\": \"https://gitlab.upgrade.invalid\", \"token\": \"glpat-upgrade-check\", \"projectQuery\": [\"none\"]}"}' >/dev/null
  echo "OK: GitLab connection"
  search 'sourcegull count:10'
  printf 'SOURCEGULL_USER=%s\nSOURCEGULL_PASSWORD=%s\nSOURCEGULL_TOKEN=%s\n' "$user" "$password" "$token" >"$STATE"
}

verify() {
  # shellcheck disable=SC1090
  . "$STATE"
  wait_ready
  sign_in /-/sign-in "{\"username\":\"$SOURCEGULL_USER\",\"password\":\"$SOURCEGULL_PASSWORD\"}"
  echo "OK: signed in with the password set before the upgrade"
  local out
  out=$(gql 'query { currentUser { username siteAdmin } site { productVersion } }')
  echo "$out" | grep -q '"siteAdmin":true' || fail "not site admin after the upgrade: $out"
  echo "OK: GraphQL ($(echo "$out" | jstr productVersion))"
  out=$(AUTH="token $SOURCEGULL_TOKEN" gql 'query { currentUser { username } }')
  [ "$(echo "$out" | jstr username)" = "$SOURCEGULL_USER" ] || fail "the old access token does not work: $out"
  echo "OK: access token from before the upgrade"
  out=$(gql 'query { savedSearches(first: 50) { nodes { description query } } }')
  echo "$out" | grep -qF "\"query\":\"$SAVED_QUERY\"" || fail "saved search missing: $out"
  echo "OK: saved search"
  out=$(gql 'query { executorSecrets(scope: BATCHES) { nodes { key } } }')
  echo "$out" | grep -q "\"key\":\"$SECRET_KEY\"" || fail "executor secret missing: $out"
  echo "OK: executor secret"
  # The config is decrypted (and redacted) to answer this.
  out=$(gql 'query { externalServices(first: 50) { nodes { displayName config } } }')
  echo "$out" | grep -q "\"displayName\":\"$CONNECTION\"" || fail "GitLab connection missing: $out"
  echo "$out" | grep -q 'gitlab.upgrade.invalid' || fail "GitLab connection config not readable: $out"
  echo "OK: GitLab connection (config decrypts)"
  search 'sourcegull count:10'
  search "$SAVED_QUERY"
  # Inserted by scripts/test/upgrade/legacy-monitor.sql with a plaintext URL.
  out=$(gql 'query { currentUser { monitors(first: 10) { nodes { description actions { nodes { ... on MonitorWebhook { url } } } } } } }')
  echo "$out" | grep -q '"url":"https://hooks.upgrade.invalid/upgrade-check-T0KEN"' ||
    fail "code monitor webhook URL not readable: $out"
  echo "OK: code monitor webhook URL from before out-of-band migration 2"

  echo "== waiting for the out-of-band migrations (up to ${OOB_TIMEOUT}s)"
  local start=$SECONDS progress
  while :; do
    out=$(gql 'query { outOfBandMigrations { id description progress applyReverse errors { message } } }')
    progress=$(echo "$out" | grep -o '"progress":[0-9.eE+-]*' | sed 's/"progress"://')
    [ -n "$progress" ] || fail "no out-of-band migrations registered: $out"
    if ! echo "$progress" | grep -qv '^1\(\.0*\)\?$'; then
      break
    fi
    [ $((SECONDS - start)) -lt "$OOB_TIMEOUT" ] || fail "out-of-band migrations incomplete: $out"
    sleep 5
  done
  echo "$out" | grep -q '"applyReverse":true' && fail "an out-of-band migration runs in reverse: $out"
  if echo "$out" | grep -o '"errors":\[[^]]*\]' | grep -qv '"errors":\[\]'; then
    fail "out-of-band migration errors: $out"
  fi
  echo "OK: $(echo "$progress" | wc -l) out-of-band migration(s) at 100% after $((SECONDS - start)) s"
}

case "$MODE" in
  seed) seed ;;
  verify) verify ;;
  *) fail "unknown mode $MODE" ;;
esac
