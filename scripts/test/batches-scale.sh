#!/usr/bin/env bash
# Batch changes at scale: a 1,000-repository batch change campaign end to end against GitLab CE,
# in Docker:
#
#   - the dev GitLab (scripts/dev gitlab up) with group batches-scale holding $BATCHES_SCALE_REPOS
#     (default 1000) small projects batches-scale/repo-0000… (README.md and config.txt, created
#     in parallel through the API) and a system hook for merge request events pointed at
#     Sourcegull's /.api/gitlab-webhooks
#   - Sourcegull from the production image (deploy/docker/Dockerfile): container sourcegull-batches-scale
#     (`sourcegull supervise`, all roles) on the dev network against the dev Postgres (database
#     sourcegull_batches_scale, recreated by `up`), on http://localhost:7082 from the host
#   - an executor (container sourcegull-batches-scale-executor, same image) on the `batches` queue
#     with the Docker runtime on the host's daemon
#
# scripts/test/batches-scale/check.py configures Sourcegull (GitLab connection with the webhook
# secret, the root token as the site credential for batch changes), waits until every repository is
# mirrored and matched by search, then runs the campaign: a server-side batch spec on
# `repositoriesMatchingQuery` (alpine + sed on config.txt, published) executed with auto-apply,
# and checks through GraphQL and the GitLab API that every changeset is an open merge request
# with the right branch, title and diff; then a bulk comment on 50 and a bulk merge of 10,
# merge requests closed in GitLab turning CLOSED through the webhook, a re-apply with a narrower
# query closing and archiving the dropped changesets, the burndown, and `gull bulk apply` on a
# small subset (the local path). Timings go to .test-out/batches-scale/results.json.
#
# Usage: scripts/test/batches-scale.sh [all|up|run|verify|down|status]
#        (or scripts/dev test-batches-scale)
#   all     (default) up, then run
#   up      GitLab and its seed, the image build, a fresh Sourcegull and executor, setup
#   run     the campaign and the checks (after up; each run uses a new batch change and branch)
#   verify  only re-checks the last run's batch change (open MRs, archive, burndown)
#   down    removes Sourcegull and the executor, stops GitLab (the seeded projects are kept)
# Environment: BATCHES_SCALE_REPOS (1000), BATCHES_SCALE_EXECUTOR_JOBS (parallel workspaces, 8),
# SKIP_BUILD=1 (reuse sourcegull/sourcegull:$BATCHES_SCALE_IMAGE_TAG, default test-batches-scale),
# BATCHES_SCALE_TIMEOUT (seconds per wait, 3600).
# Needs about 8 GB of memory (GitLab CE ~4-5 GB).
set -euo pipefail
cd "$(dirname "$0")/../.."
ROOT=$(pwd)
TAG=${BATCHES_SCALE_IMAGE_TAG:-test-batches-scale}
IMAGE=sourcegull/sourcegull:$TAG
NET=sourcegull-dev_default
SOURCEGULL=sourcegull-batches-scale
EXECUTOR=sourcegull-batches-scale-executor
DB=sourcegull_batches_scale
SOURCEGULL_URL=http://$SOURCEGULL:7080
HOST_PORT=${BATCHES_SCALE_HTTP_PORT:-7082}
GITLAB_TOKEN=glpat-sourcegull-dev-root
WORK=$ROOT/.test-out/batches-scale
mkdir -p "$WORK"
# The executor's job token (executors.accessToken), fixed per working directory.
[ -s "$WORK/executor-token" ] || head -c 24 /dev/urandom | od -An -tx1 | tr -d ' \n' > "$WORK/executor-token"
EXECUTOR_TOKEN=$(cat "$WORK/executor-token")

say() { printf "[%s] %s\n" "$(date -u +%H:%M:%S)" "$*"; }
# check.py in the toolbox container on the dev network (it reaches gitlab.localhost, sourcegull-batches-scale
# and Postgres).
check() {
  docker compose run --rm --no-deps -T -e WORK=/src/.test-out/batches-scale -e SOURCEGULL_URL="$SOURCEGULL_URL" \
    -e EXECUTOR_TOKEN="$EXECUTOR_TOKEN" -e GITLAB_TOKEN="$GITLAB_TOKEN" \
    -e BATCHES_SCALE_REPOS="${BATCHES_SCALE_REPOS:-1000}" -e BATCHES_SCALE_TIMEOUT="${BATCHES_SCALE_TIMEOUT:-3600}" -e BATCHES_SCALE_DB="$DB" \
    dev python3 -u scripts/test/batches-scale/check.py "$@"
}
psql_dev() { docker compose exec -T postgres psql -U sourcegull -v ON_ERROR_STOP=1 -q "$@"; }
ip_of() { docker inspect -f "{{(index .NetworkSettings.Networks \"$NET\").IPAddress}}" "$1"; }

gitlab_up() {
  say "GitLab"
  scripts/dev gitlab up >/dev/null
  [ -n "$(docker compose ps -q --status running postgres)" ] || docker compose up -d postgres >/dev/null
}

image() {
  if [ -z "${SKIP_BUILD:-}" ]; then
    say "building $IMAGE (deploy/docker/Dockerfile)"
    local t0=$SECONDS
    docker buildx build -q -f deploy/docker/Dockerfile --load -t "$IMAGE" \
      --build-arg REVISION="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)" . >/dev/null
    say "built in $((SECONDS - t0)) s"
    echo $((SECONDS - t0)) > "$WORK/build-seconds"
  fi
  docker image inspect alpine:3.20 >/dev/null 2>&1 || docker pull -q alpine:3.20 >/dev/null
}

sourcegull_up() {
  say "fresh Sourcegull ($SOURCEGULL, database $DB) and executor ($EXECUTOR) from $IMAGE"
  docker rm -f "$SOURCEGULL" "$EXECUTOR" >/dev/null 2>&1 || true
  docker volume rm "$SOURCEGULL-data" >/dev/null 2>&1 || true
  psql_dev -c "DROP DATABASE IF EXISTS $DB WITH (FORCE)" -c "CREATE DATABASE $DB"
  # Repo nodes clone and push to GitLab's URLs (http://gitlab.localhost:8929/...), which git
  # (libcurl) would send to the loopback address: pin the name to GitLab's address.
  local gitlab_ip
  gitlab_ip=$(ip_of "$(docker compose ps -q gitlab)")
  docker run -d --name "$SOURCEGULL" --network "$NET" -p "$HOST_PORT:7080" \
    -v "$SOURCEGULL-data:/var/opt/sourcegull" \
    -e SOURCEGULL_DATABASE_URL="postgres://sourcegull:sourcegull@postgres:5432/$DB" \
    -e SOURCEGULL_EXTERNAL_URL="$SOURCEGULL_URL" -e SOURCEGULL_LOG_FORMAT=json -e RUST_LOG=info \
    -e GIT_CONFIG_COUNT=1 -e GIT_CONFIG_KEY_0=http.curloptResolve \
    -e GIT_CONFIG_VALUE_0="gitlab.localhost:8929:$gitlab_ip" \
    "$IMAGE" supervise >/dev/null
  # Batch steps run as containers on the host's Docker (copy mode: the working copy is copied
  # into each step's container), with the image's executor on the `batches` queue only.
  docker run -d --name "$EXECUTOR" --network "$NET" \
    --group-add "$(stat -c %g /var/run/docker.sock)" -v /var/run/docker.sock:/var/run/docker.sock \
    --tmpfs /tmp:size=4g \
    -e SOURCEGULL_EXECUTOR_FRONTEND_URL="$SOURCEGULL_URL" -e SOURCEGULL_EXECUTOR_TOKEN="$EXECUTOR_TOKEN" \
    -e SOURCEGULL_EXECUTOR_QUEUES=batches -e SOURCEGULL_EXECUTOR_MAX_JOBS="${BATCHES_SCALE_EXECUTOR_JOBS:-8}" \
    -e SOURCEGULL_EXECUTOR_NAME="$EXECUTOR" -e SOURCEGULL_LOG_FORMAT=json \
    "$IMAGE" executor >/dev/null
}

up() {
  gitlab_up
  image
  say "seeding GitLab (group batches-scale, ${BATCHES_SCALE_REPOS:-1000} projects, system hook)"
  check gitlab-seed
  sourcegull_up
  say "configuring Sourcegull"
  check sourcegull-setup
}

run() {
  local t0=$SECONDS
  check campaign || { check summary || true; return 1; }
  local_run
  say "run took $((SECONDS - t0)) s"
}

# The local path: `gull bulk apply` from the image built here, as a user would run it locally,
# with steps on the host's Docker (volume mode: the working copy is copied into the containers).
local_run() {
  check local-spec
  say "gull bulk apply (local path, spec in .test-out/batches-scale/local/spec.yml)"
  local t0=$SECONDS
  docker run --rm --network "$NET" --entrypoint gull \
    --group-add "$(stat -c %g /var/run/docker.sock)" -v /var/run/docker.sock:/var/run/docker.sock \
    --tmpfs /tmp:size=2g,exec -e HOME=/tmp --env-file "$WORK/local/env" \
    -v "$WORK/local:/spec:ro" "$IMAGE" \
    bulk apply -f /spec/spec.yml -workspace volume -j 8 -tmp /tmp 2>&1 | tail -n 20
  check local $((SECONDS - t0))
}

down() {
  docker rm -f "$SOURCEGULL" "$EXECUTOR" >/dev/null 2>&1 || true
  docker volume rm "$SOURCEGULL-data" >/dev/null 2>&1 || true
  psql_dev -c "DROP DATABASE IF EXISTS $DB WITH (FORCE)" >/dev/null 2>&1 || true
  scripts/dev gitlab down >/dev/null
  say "removed Sourcegull and the executor, stopped GitLab"
}

status() {
  docker ps -a --filter name="$SOURCEGULL" --format '{{.Names}} {{.Status}}'
  docker compose ps gitlab --format '{{.Name}} {{.Status}}'
}

case "${1:-all}" in
  all) up; run ;;
  up) up ;;
  run) run ;;
  verify) check verify ;;
  summary) check summary ;;
  local) local_run ;;
  down) down ;;
  status) status ;;
  *) sed -n '2,37p' "$0"; exit 2 ;;
esac
