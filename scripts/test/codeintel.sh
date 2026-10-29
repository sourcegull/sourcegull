#!/usr/bin/env bash
# Precise code navigation end to end: precise navigation on Go, TypeScript, Java, Python and
# Rust
# repositories through CI uploads and through auto-indexing on Kubernetes, end to end, in Docker:
#
#   - the dev GitLab (scripts/dev gitlab up) with group codeintel: per language,
#     codeintel/ci-<lang> (a pinned upstream release plus a .gitlab-ci.yml including
#     deploy/gitlab-ci/sourcegull-scip.gitlab-ci.yml from the project codeintel/sourcegull-ci) and
#     codeintel/auto-<lang> (the release only)
#   - a GitLab runner (container sourcegull-codeintel-runner, Docker executor on the host daemon, jobs
#     on the dev network) running the pipelines: the upstream indexer, then `gull code-intel upload`
#     from the image built here, authenticated by CI_JOB_TOKEN
#   - Sourcegull from the production image (deploy/docker/Dockerfile), installed with the Helm chart
#     into k3s (container sourcegull-k3s, scripts/dev k3s up) in namespace sourcegull-codeintel:
#     bundled Postgres, the blob StatefulSet, and the executor with the Kubernetes runtime on
#     the codeintel queue (job pods in sourcegull-codeintel-jobs), reachable as
#     http://sourcegull-k3s:30080
#   - an indexing policy for codeintel/auto-*, so the auto-indexing scheduler queues index jobs
#     that the executor runs in k3s with the pinned indexer images
#
# Then scripts/test/codeintel/check.py waits for the 10 uploads (5 per path) and checks, per
# repository, that go to definition (into another file), the hover (with docs) and references
# (across files) are PRECISE; it prints a summary with the timings
# (.test-out/codeintel/results.json).
#
# Usage: scripts/test/codeintel.sh [all|up|run|verify|down|status]   (or scripts/dev test-codeintel)
#   all     (default) up, then run
#   up      GitLab, runner, k3s, image build and import, a fresh Helm install, seeding, setup
#   run     the CI pipelines and auto-indexing, then the checks (after up)
#   verify  only the checks (API and browser)
#   down    uninstalls the release and removes the runner, stops GitLab; k3s keeps running
# SKIP_BUILD=1 reuses the image sourcegull/sourcegull:$CODEINTEL_IMAGE_TAG (default test-codeintel).
# Needs about 8 GB of memory and, on the first run, ~10 GB of images (GitLab, indexers).
set -euo pipefail
cd "$(dirname "$0")/../.."
ROOT=$(pwd)
TAG=${CODEINTEL_IMAGE_TAG:-test-codeintel}
IMAGE=sourcegull/sourcegull:$TAG
NS=sourcegull-codeintel
JOBS_NS=sourcegull-codeintel-jobs
NET=sourcegull-dev_default
RUNNER=sourcegull-codeintel-runner
RUNNER_IMAGE=gitlab/gitlab-runner:v19.4.1
HELM_IMAGE=alpine/helm:3.16.4
NODE_PORT=30080
SOURCEGULL_URL=http://sourcegull-k3s:$NODE_PORT
GITLAB_TOKEN=glpat-sourcegull-dev-root
WORK=$ROOT/.test-out/codeintel
mkdir -p "$WORK"

say() { printf "[%s] %s\n" "$(date -u +%H:%M:%S)" "$*"; }
kubectl() { docker exec -i sourcegull-k3s kubectl "$@"; }
helm() {
  docker run --rm --network "$NET" -v "$WORK/kubeconfig:/kube/config:ro" -e KUBECONFIG=/kube/config \
    -v "$ROOT/deploy/helm/sourcegull:/chart:ro" -v "$ROOT/scripts/test/codeintel:/values:ro" \
    "$HELM_IMAGE" "$@" 2> >(grep -v 'configuration file is \(group\|world\)-readable' >&2)
}
# check.py in the toolbox container on the dev network (it reaches gitlab.localhost and sourcegull-k3s).
check() {
  docker compose run --rm --no-deps -T -e WORK=/src/.test-out/codeintel -e SOURCEGULL_URL="$SOURCEGULL_URL" \
    -e SOURCEGULL_IMAGE="$IMAGE" -e EXECUTOR_TOKEN="${EXECUTOR_TOKEN:-}" -e GITLAB_TOKEN="$GITLAB_TOKEN" \
    -e CODEINTEL_ONLY="${CODEINTEL_ONLY:-}" -e CODEINTEL_TIMEOUT="${CODEINTEL_TIMEOUT:-2400}" \
    dev python3 -u scripts/test/codeintel/check.py "$@"
}
gitlab_api() {
  docker compose exec -T gitlab curl -fsS -H "PRIVATE-TOKEN: $GITLAB_TOKEN" "$@"
}
ip_of() { docker inspect -f "{{(index .NetworkSettings.Networks \"$NET\").IPAddress}}" "$1"; }

infra_up() {
  say "GitLab"
  scripts/dev gitlab up >/dev/null
  say "k3s"
  scripts/dev k3s up >/dev/null
  docker exec sourcegull-k3s cat /etc/rancher/k3s/k3s.yaml |
    sed 's#https://127.0.0.1:6443#https://sourcegull-k3s:6443#' > "$WORK/kubeconfig"
  # Pods resolve the containers of the dev network through CoreDNS (k3s's own resolver cannot
  # reach Docker's).
  local gitlab_ip k3s_ip
  gitlab_ip=$(ip_of "$(docker compose ps -q gitlab)")
  k3s_ip=$(ip_of sourcegull-k3s)
  kubectl apply -f - >/dev/null <<EOF
apiVersion: v1
kind: ConfigMap
metadata: { name: coredns-custom, namespace: kube-system }
data:
  sourcegull-codeintel.server: |
    gitlab.localhost:53 sourcegull-k3s:53 {
      hosts {
        $gitlab_ip gitlab.localhost
        $k3s_ip sourcegull-k3s
      }
    }
EOF
  kubectl -n kube-system rollout restart deploy coredns >/dev/null
  runner_up
}

runner_up() {
  if [ "$(docker inspect -f '{{.State.Running}}' "$RUNNER" 2>/dev/null)" = true ]; then
    return
  fi
  say "GitLab runner ($RUNNER)"
  docker rm -f "$RUNNER" >/dev/null 2>&1 || true
  docker volume rm "$RUNNER-config" >/dev/null 2>&1 || true
  local token id
  for id in $(gitlab_api "http://localhost:8929/api/v4/runners/all?per_page=100" |
      jq -r --arg d "$RUNNER" '.[] | select(.description == $d) | .id'); do
    gitlab_api -X DELETE "http://localhost:8929/api/v4/runners/$id" >/dev/null
  done
  token=$(gitlab_api -X POST http://localhost:8929/api/v4/user/runners \
    -d runner_type=instance_type -d run_untagged=true -d description="$RUNNER" | jq -r .token)
  docker run -d --name "$RUNNER" --network "$NET" --restart unless-stopped \
    -v /var/run/docker.sock:/var/run/docker.sock -v "$RUNNER-config:/etc/gitlab-runner" \
    "$RUNNER_IMAGE" >/dev/null
  # Job containers join the dev network and upload to sourcegull-k3s. The runner talks to GitLab as
  # http://gitlab:8929: git (libcurl) resolves gitlab.localhost to the loopback address. Local
  # images (the Sourcegull image built here) are used as they are.
  docker exec "$RUNNER" gitlab-runner register --non-interactive --url http://gitlab:8929 \
    --token "$token" --executor docker --docker-image alpine:3.20 \
    --docker-network-mode "$NET" --docker-pull-policy if-not-present \
    --clone-url http://gitlab:8929 >/dev/null 2>&1
  # Four jobs at a time, and as many job requests, so that pipelines start without waiting for
  # a long poll to return.
  docker exec "$RUNNER" sed -i -e 's/^concurrent = .*/concurrent = 4/' \
    -e 's/^\(  url = .*\)/\1\n  request_concurrency = 4/' /etc/gitlab-runner/config.toml
  docker restart "$RUNNER" >/dev/null
}

image() {
  if [ -z "${SKIP_BUILD:-}" ]; then
    say "building $IMAGE (deploy/docker/Dockerfile)"
    local t0=$SECONDS
    scripts/image-build.sh -q -f deploy/docker/Dockerfile --load -t "$IMAGE" \
      --build-arg REVISION="$(git rev-parse --short HEAD 2>/dev/null || echo unknown)" . >/dev/null
    say "built in $((SECONDS - t0)) s"
  fi
  say "importing $IMAGE into k3s"
  docker save "$IMAGE" | docker exec -i sourcegull-k3s ctr -n k8s.io images import - >/dev/null
  # The indexer images, pulled once into k3s (like a node with a warm cache), so the job timings
  # do not include multi-GB pulls. The runner uses the host's copies.
  local img
  for img in $(grep -o 'sourcegraph/scip-\(go\|java\|python\|rust\|typescript\)@sha256:[0-9a-f]*' \
      crates/autoindex/src/indexers.rs); do
    docker exec sourcegull-k3s crictl inspecti "$img" >/dev/null 2>&1 ||
      { say "pulling $img into k3s"; docker exec sourcegull-k3s crictl pull "$img" >/dev/null; }
    docker image inspect "$img" >/dev/null 2>&1 || docker pull -q "$img" >/dev/null
  done
}

install() {
  say "fresh Helm install of deploy/helm/sourcegull in namespace $NS"
  helm uninstall sourcegull -n "$NS" --wait >/dev/null 2>&1 || true
  kubectl delete namespace "$NS" "$JOBS_NS" --ignore-not-found --wait >/dev/null
  kubectl create namespace "$NS" >/dev/null
  kubectl create namespace "$JOBS_NS" >/dev/null
  local t0=$SECONDS
  # Repo nodes clone from GitLab's clone URLs (http://gitlab.localhost:8929/...), which git
  # (libcurl) would send to the loopback address: pin the name to GitLab's address.
  local gitlab_ip
  gitlab_ip=$(ip_of "$(docker compose ps -q gitlab)")
  helm upgrade --install sourcegull /chart -n "$NS" -f /values/values.yaml \
    --set image.tag="$TAG" --set externalURL="$SOURCEGULL_URL" \
    --set-json "extraEnvVars=[{\"name\":\"GIT_CONFIG_COUNT\",\"value\":\"1\"},
      {\"name\":\"GIT_CONFIG_KEY_0\",\"value\":\"http.curloptResolve\"},
      {\"name\":\"GIT_CONFIG_VALUE_0\",\"value\":\"gitlab.localhost:8929:$gitlab_ip\"}]" \
    --wait --timeout 10m >/dev/null
  say "release ready in $((SECONDS - t0)) s"
  kubectl -n "$NS" get pods
}

executor_token() {
  kubectl -n "$NS" get secret sourcegull-secrets -o jsonpath='{.data.executor-token}' | base64 -d
}

up() {
  infra_up
  image
  install
  say "seeding GitLab"
  check gitlab-seed
  say "configuring Sourcegull"
  EXECUTOR_TOKEN=$(executor_token) check sourcegull-setup
}

run() {
  local t0=$SECONDS rc=0
  check run || rc=$?
  say "run took $((SECONDS - t0)) s"
  browser || rc=$?
  return $rc
}

# The file view of an auto-indexed repository in Chromium (hover, references, go to definition).
browser() {
  docker image inspect sourcegull-dev-e2e:latest >/dev/null 2>&1 ||
    docker compose --profile e2e build e2e >/dev/null
  say "browser check (scripts/test/codeintel/hover.cjs; screenshots in .test-out/codeintel/)"
  docker run --rm --network "$NET" -u "$(id -u):$(id -g)" -v "$ROOT:/src" -w /src \
    -e BASE="$SOURCEGULL_URL" -e OUT=/src/.test-out/codeintel sourcegull-dev-e2e:latest \
    node scripts/test/codeintel/hover.cjs
}

down() {
  helm uninstall sourcegull -n "$NS" --wait >/dev/null 2>&1 || true
  kubectl delete namespace "$NS" "$JOBS_NS" --ignore-not-found >/dev/null || true
  kubectl -n kube-system delete configmap coredns-custom --ignore-not-found >/dev/null || true
  docker rm -f "$RUNNER" >/dev/null 2>&1 || true
  scripts/dev gitlab down >/dev/null
  say "removed the release and the runner, stopped GitLab (k3s keeps running)"
}

status() {
  kubectl -n "$NS" get pods
  kubectl -n "$JOBS_NS" get jobs,pods
  docker ps --filter name="$RUNNER" --format '{{.Names}} {{.Status}}'
}

case "${1:-all}" in
  all) up; run ;;
  up) up ;;
  run) run ;;
  verify) check verify && browser ;;
  down) down ;;
  status) status ;;
  *) sed -n '2,32p' "$0"; exit 2 ;;
esac
