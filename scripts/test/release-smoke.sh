#!/usr/bin/env bash
# Smoke test of a built image, for any platform Docker can run (another architecture needs QEMU
# registered on the host):
# Postgres in a container and `supervise` (migrations, then every role); waits for /readyz and
# loads the sign-up page. The release workflow runs it before pushing.
#
# Usage: scripts/test/release-smoke.sh <image> [platform]   e.g. linux/arm64
#   SMOKE_TIMEOUT  seconds to wait for /readyz (default 300; emulation is slow)
set -euo pipefail
IMAGE=${1:?usage: release-smoke.sh <image> [platform]}
PLATFORM=${2:-}
TIMEOUT=${SMOKE_TIMEOUT:-300}
NAME=sourcegull-smoke-$$
NET=$NAME
PG=$NAME-pg
APP=$NAME-app
PLAT=()
[ -n "$PLATFORM" ] && PLAT=(--platform "$PLATFORM")

cleanup() {
  status=$?
  if [ "$status" != 0 ]; then
    echo "--- sourcegull logs" >&2
    docker logs --tail 200 "$APP" >&2 2>&1 || true
  fi
  docker rm -f "$APP" "$PG" >/dev/null 2>&1 || true
  docker network rm "$NET" >/dev/null 2>&1 || true
  exit "$status"
}
trap cleanup EXIT

docker network create "$NET" >/dev/null
docker run -d --name "$PG" --network "$NET" \
  -e POSTGRES_USER=sourcegull -e POSTGRES_PASSWORD=sourcegull -e POSTGRES_DB=sourcegull \
  postgres:18-bookworm >/dev/null
until docker exec "$PG" pg_isready -U sourcegull -d sourcegull -h 127.0.0.1 >/dev/null 2>&1; do sleep 1; done

DB=postgres://sourcegull:sourcegull@$PG:5432/sourcegull?sslmode=disable
KEYS="k1:$(head -c 32 /dev/urandom | base64)"
# No published port: the checks run curl inside the network, so parallel runs do not clash.
docker run -d --name "$APP" ${PLAT[@]+"${PLAT[@]}"} --network "$NET" --network-alias sourcegull \
  -e SOURCEGULL_DATABASE_URL="$DB" -e SOURCEGULL_KEYS="$KEYS" "$IMAGE" supervise >/dev/null

curl_() { docker run --rm --network "$NET" curlimages/curl:8.11.1 -sS "$@"; }
deadline=$((SECONDS + TIMEOUT))
until [ "$(curl_ -o /dev/null -w '%{http_code}' http://sourcegull:6060/readyz 2>/dev/null)" = 200 ]; do
  if [ "$(docker inspect -f '{{.State.Running}}' "$APP")" != true ]; then
    echo "sourcegull exited" >&2
    exit 1
  fi
  if [ "$SECONDS" -ge "$deadline" ]; then
    echo "/readyz not ready after ${TIMEOUT}s" >&2
    exit 1
  fi
  sleep 3
done
echo "ready: /readyz"

page=$(curl_ -f http://sourcegull:7080/sign-up)
grep -q '<html' <<<"$page" || { echo "/sign-up did not return the web UI" >&2; exit 1; }
echo "ok: /sign-up"
echo "smoke test passed: $IMAGE ${PLATFORM:-native}"
