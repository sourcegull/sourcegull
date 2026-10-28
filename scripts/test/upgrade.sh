#!/usr/bin/env bash
# Upgrade test: upgrade version N-2 to N with Docker Compose and with the Helm chart, and check
# that the data survives and the app works, and that the previous release N-1 still works on
# N's schema (it keeps running there during a rolling upgrade). In Docker.
#
# Versions. N is this checkout (its images built from the working tree). N-2 is a git ref:
# SOURCEGULL_UPGRADE_FROM, else the second newest release tag (`v*`, not on HEAD) once two exist,
# else the newest one. Before the first release there is no N-2: build, compose and helm skip
# (exit 0) with a message, and source fails. N-1 is SOURCEGULL_UPGRADE_PREVIOUS, else the newest
# release tag not on HEAD, else the commit HEAD branched from on the default branch (origin/HEAD,
# else origin/main), else HEAD's parent. Each image is built from its own tree with its own
# Dockerfile.
#
#   scripts/test/upgrade.sh refs      print N-2, N-1 and N
#   scripts/test/upgrade.sh contract  apply N-1's and N's migrations to scratch databases (a
#                                      postgres container) and fail when N removes a column,
#                                      table or allowed value that N-1's code still names
#                                      (upgrade/contract.sh)
#   scripts/test/upgrade.sh build     build sourcegull/sourcegull:upgrade-from (N-2), :upgrade-previous
#                                      (N-1) and :upgrade-to (N) when missing
#                                      (SOURCEGULL_UPGRADE_BUILD=1: always)
#   scripts/test/upgrade.sh source D [REF]
#                                      write N-2's (or REF's) tree to directory D (CI builds it
#                                      from there)
#   scripts/test/upgrade.sh compose   N-2's compose.yaml and image; seed data (smoke.sh seed);
#                                      stop, rotate the encryption key, then the documented
#                                      multi-version upgrade with N's compose.yaml and image
#                                      (`migrate versions`, `migrate upgrade --from --dry-run`,
#                                      `migrate upgrade --from`, `up`); smoke.sh verify (old
#                                      password and token, saved search, secrets, search,
#                                      out-of-band migrations at 100%), `migrate drift` clean,
#                                      no secret left on a retired key; then N-1's image on N's
#                                      schema (no migrations run) and smoke.sh verify again
#   scripts/test/upgrade.sh helm      on kind: N-2's chart and image, `helm test`, seed; `helm
#                                      upgrade` to N's chart and image (migrations in the
#                                      pre-upgrade hook / init container); `helm test` with the
#                                      sign-in/GraphQL/search smoke test as the seeded admin;
#                                      smoke.sh verify; `migrate drift` clean
#   scripts/test/upgrade.sh all       contract, build, compose, helm
#   scripts/test/upgrade.sh down      remove the Compose project and the kind cluster
#
# Environment: SOURCEGULL_UPGRADE_FROM (git ref of N-2), SOURCEGULL_UPGRADE_PREVIOUS (git ref of N-1),
# SOURCEGULL_UPGRADE_BUILD=1, CARGO_BUILD_JOBS
# (rustc jobs of the image builds, default 3), SOURCEGULL_UPGRADE_KEEP=1 (keep the stack after a
# run), OOB_TIMEOUT (seconds to wait for the out-of-band migrations, default 600).
# Work files go to .test-out/upgrade/ (gitignored).
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT=$(pwd)
W=$ROOT/.test-out/upgrade
mkdir -p "$W"

# After the schema upgrade, legacy-monitor.sql's plaintext code monitor webhook (as stored before
# out-of-band migration 2) is inserted; after the services ran, it must be encrypted.
ENCRYPTED_SQL="SELECT count(*) FROM cm_actions WHERE url = '' AND url_enc IS NOT NULL"

FROM_IMAGE=sourcegull/sourcegull:upgrade-from
PREV_IMAGE=sourcegull/sourcegull:upgrade-previous
TO_IMAGE=sourcegull/sourcegull:upgrade-to
PROJECT=sourcegull-upgrade-test
CLUSTER=sourcegull-upgrade-test
NAMESPACE=sourcegull-upgrade
KIND_VERSION=v0.29.0
# bash, curl, kubectl and helm: runs smoke.sh and drives the kind cluster.
TOOLS_IMAGE=quay.io/helmpack/chart-testing:v3.12.0
AUX_IMAGES=(postgres:18.6-bookworm busybox:1.37 curlimages/curl:8.11.1)

log() { echo "== $*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }

# The git ref of N-2; nothing before the first release.
from_ref() {
  if [ -n "${SOURCEGULL_UPGRADE_FROM:-}" ]; then
    git rev-parse --verify "$SOURCEGULL_UPGRADE_FROM^{commit}"
    return
  fi
  local tag
  tag=$(git tag -l 'v[0-9]*' --sort=-v:refname --no-contains HEAD | head -n 2 | tail -n 1)
  [ -z "$tag" ] || git rev-parse --verify "$tag^{commit}"
}

NO_FROM="no version to upgrade from: there is no release tag (v*) yet and SOURCEGULL_UPGRADE_FROM is unset"

# Exits 0 with a message when there is no N-2 (before the first release).
need_from() {
  [ -n "$(from_ref)" ] && return
  echo "SKIP: $NO_FROM"
  exit 0
}

# The git ref of N-1.
prev_ref() {
  if [ -n "${SOURCEGULL_UPGRADE_PREVIOUS:-}" ]; then
    git rev-parse --verify "$SOURCEGULL_UPGRADE_PREVIOUS^{commit}"
    return
  fi
  local tag base
  tag=$(git tag -l 'v[0-9]*' --sort=-v:refname --no-contains HEAD | head -n 1)
  if [ -n "$tag" ]; then
    git rev-parse --verify "$tag^{commit}"
    return
  fi
  for base in origin/HEAD origin/main; do
    base=$(git merge-base HEAD "$base" 2>/dev/null) || continue
    if [ "$base" != "$(git rev-parse HEAD)" ]; then
      echo "$base"
      return
    fi
    break
  done
  git rev-parse --verify 'HEAD~1^{commit}'
}

# The newest main migration of a commit (what `migrate upgrade --from` takes for N-2 until it
# is a release).
last_migration() {
  git ls-tree --name-only "$1" crates/db/migrations/main/ | sed -n 's#.*/\([0-9]*\)_.*\.up\.sql#\1#p' | sort -n | tail -n 1
}

cmd_refs() {
  local from
  from=$(from_ref)
  if [ -n "$from" ]; then
    echo "N-2: $(git log -1 --format='%h %cs %s' "$from") (newest migration $(last_migration "$from"))"
  else
    echo "N-2: none ($NO_FROM)"
  fi
  local prev
  prev=$(prev_ref)
  echo "N-1: $(git log -1 --format='%h %cs %s' "$prev") (newest migration $(last_migration "$prev"))"
  echo "N:   $(git log -1 --format='%h %cs %s' HEAD) (working tree; newest migration $(last_migration HEAD))"
}

# Writes N-2's tree (or that of the ref $2) to $1. Older Dockerfiles lack the CARGO_BUILD_JOBS
# argument: add it (it only caps rustc's parallelism).
cmd_source() {
  local dir=${1:?directory} from=${2:-}
  [ -n "$from" ] && from=$(git rev-parse --verify "$from^{commit}") || from=$(from_ref)
  [ -n "$from" ] || fail "$NO_FROM"
  rm -rf "$dir" && mkdir -p "$dir"
  git archive "$from" | tar -x -C "$dir"
  grep -q '^ARG CARGO_BUILD_JOBS' "$dir/deploy/docker/Dockerfile" ||
    sed -i 's/^FROM chef AS builder$/&\nARG CARGO_BUILD_JOBS/' "$dir/deploy/docker/Dockerfile"
  echo "$from" >"$dir/.upgrade-ref"
}

build_image() {
  local tag=$1 dir=$2 rev=$3
  if [ "${SOURCEGULL_UPGRADE_BUILD:-0}" != 1 ] && docker image inspect "$tag" >/dev/null 2>&1; then
    echo "$tag exists (SOURCEGULL_UPGRADE_BUILD=1 rebuilds it)"
    return
  fi
  log "building $tag from $rev"
  local start=$SECONDS
  docker buildx build -f "$dir/deploy/docker/Dockerfile" --load -t "$tag" \
    --build-arg CARGO_BUILD_JOBS="${CARGO_BUILD_JOBS:-3}" --build-arg REVISION="$rev" "$dir"
  echo "built $tag in $((SECONDS - start)) s"
}

cmd_build() {
  need_from
  local from
  from=$(from_ref)
  if [ "${SOURCEGULL_UPGRADE_BUILD:-0}" = 1 ] || ! docker image inspect "$FROM_IMAGE" >/dev/null 2>&1; then
    cmd_source "$W/src-from"
  fi
  build_image "$FROM_IMAGE" "$W/src-from" "$from"
  local prev
  prev=$(prev_ref)
  if [ "$prev" = "$from" ]; then
    docker tag "$FROM_IMAGE" "$PREV_IMAGE"
  else
    if [ "${SOURCEGULL_UPGRADE_BUILD:-0}" = 1 ] || ! docker image inspect "$PREV_IMAGE" >/dev/null 2>&1; then
      cmd_source "$W/src-previous" "$prev"
    fi
    build_image "$PREV_IMAGE" "$W/src-previous" "$prev"
  fi
  build_image "$TO_IMAGE" "$ROOT" "$(git rev-parse HEAD)"
}

# Runs upgrade/contract.sh against a throwaway postgres container.
cmd_contract() {
  local name=$PROJECT-contract prev
  prev=$(prev_ref)
  docker rm -f "$name" >/dev/null 2>&1 || true
  docker run -d --name "$name" -e POSTGRES_HOST_AUTH_METHOD=trust postgres:18-bookworm >/dev/null
  # shellcheck disable=SC2064
  trap "docker rm -f $name >/dev/null 2>&1" RETURN
  local start=$SECONDS
  until docker exec "$name" pg_isready -qU postgres -h 127.0.0.1; do
    [ $((SECONDS - start)) -lt 60 ] || fail "the contract check's postgres did not start"
    sleep 1
  done
  log "schema contract: N-1 $(git log -1 --format=%h "$prev") on N's migrations"
  CONTRACT_PSQL="docker exec -i $name psql -U postgres -h 127.0.0.1" scripts/test/upgrade/contract.sh "$prev"
}

# Runs smoke.sh in the tools image on a Docker network.
smoke() {
  local network=$1
  shift
  docker run --rm --network "$network" -v "$ROOT/scripts/test/upgrade:/smoke:ro" -v "$W:$W" \
    -e OOB_TIMEOUT="${OOB_TIMEOUT:-600}" --entrypoint bash "$TOOLS_IMAGE" /smoke/smoke.sh "$@"
}

# wait_encrypted CMD...: CMD prints the result of $ENCRYPTED_SQL; waits until it is 1 (the
# out-of-band migration runner re-measures every 30 s when idle).
wait_encrypted() {
  local start=$SECONDS
  until [ "$("$@")" = 1 ]; do
    [ $((SECONDS - start)) -lt 180 ] || fail "the legacy code monitor webhook URL is not encrypted"
    sleep 5
  done
  echo "OK: the legacy code monitor webhook URL is encrypted at rest"
}

images_present() {
  local img
  for img in "$FROM_IMAGE" "$PREV_IMAGE" "$TO_IMAGE"; do
    docker image inspect "$img" >/dev/null 2>&1 || fail "$img is needed: run '$0 build' first"
  done
}

############################
# Docker Compose
############################

# dc DIR ARGS...: docker compose with DIR's compose.yaml and the check's .env.
dc() {
  local dir=$1
  shift
  docker compose -p "$PROJECT" --env-file "$W/compose/.env" -f "$dir/compose.yaml" "$@"
}

compose_down() {
  local dir
  for dir in "$W/compose/to" "$W/compose/from"; do
    if [ -f "$dir/compose.yaml" ]; then
      dc "$dir" --profile '*' down -v --remove-orphans >/dev/null 2>&1 && return
    fi
  done
  return 0
}

cmd_compose() {
  need_from
  images_present
  local from from_mig total=$SECONDS t
  from=$(from_ref)
  from_mig=$(last_migration "$from")
  local C=$W/compose
  compose_down
  rm -rf "$C" && mkdir -p "$C/from" "$C/to" "$C/secrets"
  git archive "$from" deploy/docker | tar -x -C "$C/from" --strip-components=2
  cp -r deploy/docker/. "$C/to/"
  printf 'k1:%s\n' "$(head -c 32 /dev/urandom | base64)" >"$C/secrets/encryption-keys"
  chmod 0644 "$C/secrets/encryption-keys"
  # A random host port (nothing else may hold 7080), small memory settings.
  cat >"$C/.env" <<EOF
SOURCEGULL_IMAGE=${FROM_IMAGE%:*}
SOURCEGULL_VERSION=${FROM_IMAGE##*:}
POSTGRES_PASSWORD=upgrade-check
SOURCEGULL_DATABASE_URL=postgres://sourcegull:upgrade-check@postgres:5432/sourcegull
SOURCEGULL_HTTP_PORT=127.0.0.1:0
SOURCEGULL_KEYS_FILE_HOST=$C/secrets/encryption-keys
POSTGRES_SHARED_BUFFERS=128MB
POSTGRES_EFFECTIVE_CACHE_SIZE=256MB
POSTGRES_MEMORY_LIMIT=1g
SOURCEGULL_REPO_MEMORY_LIMIT=2g
EOF
  local network=${PROJECT}_default

  log "N-2: $(git log -1 --format='%h %s' "$from") with its compose.yaml"
  t=$SECONDS
  dc "$C/from" up -d --wait --wait-timeout 300
  echo "up in $((SECONDS - t)) s"
  dc "$C/from" ps --format '{{.Service}} {{.Status}}'
  log "seed"
  smoke "$network" seed http://web:7080 "$W/compose-state"

  log "upgrade to N: stop, rotate the encryption key, migrate upgrade --from $from_mig, up"
  t=$SECONDS
  dc "$C/from" stop web worker repo-0
  printf 'k2:%s\n' "$(head -c 32 /dev/urandom | base64)" >>"$C/secrets/encryption-keys"
  sed -i "s/^SOURCEGULL_VERSION=.*/SOURCEGULL_VERSION=${TO_IMAGE##*:}/" "$C/.env"
  dc "$C/to" run --rm migrate migrate versions
  dc "$C/to" run --rm migrate migrate upgrade --from "$from_mig" --dry-run
  dc "$C/to" run --rm migrate migrate upgrade --from "$from_mig"
  dc "$C/to" exec -T postgres psql -qU sourcegull -d sourcegull -v ON_ERROR_STOP=1 <scripts/test/upgrade/legacy-monitor.sql
  dc "$C/to" up -d --wait --wait-timeout 300 --remove-orphans
  echo "upgraded in $((SECONDS - t)) s"
  dc "$C/to" ps --format '{{.Service}} {{.Image}} {{.Status}}'

  log "verify"
  t=$SECONDS
  smoke "$network" verify http://web:7080 "$W/compose-state"
  echo "verified in $((SECONDS - t)) s"
  wait_encrypted dc "$C/to" exec -T postgres psql -tAU sourcegull -d sourcegull -c "$ENCRYPTED_SQL"
  log "migrate drift"
  dc "$C/to" run --rm migrate migrate drift
  dc "$C/to" run --rm migrate migrate status | tail -n 8
  log "keys status"
  local keys
  keys=$(dc "$C/to" run --rm --no-deps web keys status)
  echo "$keys"
  echo "$keys" | grep -q '^current key-encryption key: k2$' || fail "k2 is not the current key"
  # Every encrypted column: nothing left on retired data keys.
  if echo "$keys" | sed -n '/^COLUMN/,$p' | tail -n +2 | awk '$3 != 0 { bad = 1 } END { exit !bad }'; then
    fail "secrets left on retired keys"
  fi
  echo "OK: no secret on a retired key"

  # A rolling upgrade runs N-1 on N's schema: start its image without the migrate service.
  log "N-1 on N's schema: $(git log -1 --format='%h %s' "$(prev_ref)")"
  t=$SECONDS
  sed -i "s/^SOURCEGULL_VERSION=.*/SOURCEGULL_VERSION=${PREV_IMAGE##*:}/" "$C/.env"
  dc "$C/to" up -d --no-deps --wait --wait-timeout 300 web worker repo-0
  dc "$C/to" ps --format '{{.Service}} {{.Image}} {{.Status}}'
  smoke "$network" verify http://web:7080 "$W/compose-state"
  echo "N-1 verified on N's schema in $((SECONDS - t)) s"
  log "Compose upgrade N-2 -> N passed in $((SECONDS - total)) s"
  if [ "${SOURCEGULL_UPGRADE_KEEP:-0}" != 1 ]; then
    compose_down
  fi
}

############################
# Helm on kind
############################

kind_bin() {
  local bin=$W/kind-$KIND_VERSION
  if [ ! -x "$bin" ]; then
    echo "downloading kind $KIND_VERSION" >&2
    local url=https://github.com/kubernetes-sigs/kind/releases/download/$KIND_VERSION/kind-linux-amd64
    curl -fsSL -o "$bin.tmp" "$url"
    echo "$(curl -fsSL "$url.sha256sum" | cut -d' ' -f1)  $bin.tmp" | sha256sum -c - >/dev/null
    chmod +x "$bin.tmp" && mv "$bin.tmp" "$bin"
  fi
  echo "$bin"
}

# Runs a shell command in the tools image on the kind network, with the repository and the work
# directory mounted at their paths.
k() {
  docker run --rm --network kind -v "$ROOT:$ROOT:ro" -v "$W:$W" -w "$ROOT" \
    -v "$W/kubeconfig:/root/.kube/config:ro" -e OOB_TIMEOUT="${OOB_TIMEOUT:-600}" \
    --entrypoint bash "$TOOLS_IMAGE" -c "set -euo pipefail; $*"
}

cmd_helm() {
  need_from
  images_present
  local from kind total=$SECONDS t
  from=$(from_ref)
  kind=$(kind_bin)
  export KUBECONFIG=$W/host-kubeconfig
  if ! "$kind" get clusters 2>/dev/null | grep -qx "$CLUSTER"; then
    log "creating kind cluster $CLUSTER"
    "$kind" create cluster --name "$CLUSTER" --wait 120s
  fi
  "$kind" get kubeconfig --name "$CLUSTER" --internal >"$W/kubeconfig"
  chmod 0600 "$W/kubeconfig"
  local img
  for img in "${AUX_IMAGES[@]}"; do
    docker image inspect "$img" >/dev/null 2>&1 || docker pull -q "$img" >/dev/null
  done
  log "loading images into $CLUSTER"
  # As in scripts/test/helm-ct.sh: ctr import, not `kind load` (multi-platform images).
  for img in "$FROM_IMAGE" "$TO_IMAGE" "${AUX_IMAGES[@]}"; do
    docker save "$img" | docker exec -i "$CLUSTER-control-plane" \
      ctr --namespace=k8s.io images import --platform linux/amd64 --digests --snapshotter=overlayfs - >/dev/null
  done
  local H=$W/helm
  rm -rf "$H" && mkdir -p "$H"
  git archive "$from" deploy/helm/sourcegull | tar -x -C "$H" --strip-components=2
  local ns=$NAMESPACE values=deploy/helm/ct/kind-values.yaml from_values=$W/kind-values-from.yaml
  k "kubectl delete namespace $ns --ignore-not-found --wait"
  # Each chart version gets kind-values.yaml as of its own commit (value names change across
  # breaking chart versions).
  git show "$from:deploy/helm/ct/kind-values.yaml" >"$from_values"
  local pf="kubectl -n $ns port-forward svc/sourcegull-web 17080:7080 >/dev/null & pf=\$!; trap 'kill \$pf' EXIT; sleep 3"

  log "N-2: chart $(grep '^version:' "$H/sourcegull/Chart.yaml") from $(git log -1 --format=%h "$from"), image $FROM_IMAGE"
  t=$SECONDS
  k "helm install sourcegull $H/sourcegull -n $ns --create-namespace -f $from_values \
      --set image.tag=${FROM_IMAGE##*:} --wait --timeout 900s && helm test sourcegull -n $ns --timeout 300s"
  echo "installed in $((SECONDS - t)) s"
  log "seed"
  k "$pf; bash scripts/test/upgrade/smoke.sh seed http://127.0.0.1:17080 $W/helm-state"
  # The smoke helm test of N signs in as the seeded admin.
  k ". $W/helm-state; kubectl -n $ns create secret generic upgrade-smoke \
      --from-literal=username=\$SOURCEGULL_USER --from-literal=password=\$SOURCEGULL_PASSWORD"

  log "helm upgrade to N: chart $(grep '^version:' deploy/helm/sourcegull/Chart.yaml), image $TO_IMAGE"
  t=$SECONDS
  k "helm upgrade sourcegull deploy/helm/sourcegull -n $ns -f $values --set image.tag=${TO_IMAGE##*:} \
      --set tests.smoke.enabled=true --set tests.smoke.credentialsSecret=upgrade-smoke \
      --wait --timeout 900s && kubectl -n $ns get pods"
  echo "upgraded in $((SECONDS - t)) s"
  local psql="kubectl -n $ns exec -i sourcegull-postgresql-0 -c postgresql -- psql -U sourcegull -d sourcegull -v ON_ERROR_STOP=1"
  k "$psql -q <scripts/test/upgrade/legacy-monitor.sql"
  log "helm test (readyz, sign-in/GraphQL/search smoke)"
  k "helm test sourcegull -n $ns --timeout 300s"
  log "verify"
  t=$SECONDS
  k "$pf; bash scripts/test/upgrade/smoke.sh verify http://127.0.0.1:17080 $W/helm-state"
  echo "verified in $((SECONDS - t)) s"
  wait_encrypted k "$psql -tAc \"$ENCRYPTED_SQL\""
  log "migrate drift"
  k "kubectl -n $ns exec deploy/sourcegull-web -c web -- sourcegull migrate drift && \
     kubectl -n $ns exec deploy/sourcegull-web -c web -- sourcegull migrate status | tail -n 8"
  log "Helm upgrade N-2 -> N passed in $((SECONDS - total)) s"
  if [ "${SOURCEGULL_UPGRADE_KEEP:-0}" != 1 ]; then
    "$kind" delete cluster --name "$CLUSTER"
  fi
}

cmd_down() {
  compose_down
  if [ -x "$W/kind-$KIND_VERSION" ]; then
    KUBECONFIG=$W/host-kubeconfig "$W/kind-$KIND_VERSION" delete cluster --name "$CLUSTER"
  fi
}

case "${1:-}" in
  refs) cmd_refs ;;
  source) cmd_source "${2:-}" "${3:-}" ;;
  contract) cmd_contract ;;
  build) cmd_build ;;
  compose) cmd_compose ;;
  helm) cmd_helm ;;
  all) cmd_refs; cmd_contract; cmd_build; cmd_compose; cmd_helm ;;
  down) cmd_down ;;
  *) sed -n '2,46p' "$0" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
