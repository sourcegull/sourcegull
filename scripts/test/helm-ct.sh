#!/usr/bin/env bash
# Helm chart checks, all in Docker:
#
#   scripts/test/helm-ct.sh template   values.schema.json is current (scripts/helm-schema.py), then
#                                      helm lint --strict + helm template for the default values, every
#                                      sizing preset (examples/*.yaml), every ci/*-values.yaml, every
#                                      option at once (ct/options-values.yaml) and the option matrix
#                                      (KEDA, CNPG, worker HPA, S3, executor, ...)
#   scripts/test/helm-ct.sh lint       chart-testing `ct lint` (yamllint, schema, version bump vs main)
#   scripts/test/helm-ct.sh install    `ct install --upgrade` on a kind cluster: for each
#                                      ci/*-values.yaml a fresh install + `helm test` (readyz and the
#                                      sign-in/GraphQL/search smoke test), and an install of the
#                                      previous chart version (from origin/main) upgraded to this one
#   scripts/test/helm-ct.sh all        template, lint and install
#   scripts/test/helm-ct.sh down       delete the kind cluster
#
# Environment:
#   SOURCEGULL_CT_IMAGE        Sourcegull image to test (default sourcegull/sourcegull:ct). Built from
#                              deploy/docker/Dockerfile when missing, or always with SOURCEGULL_CT_BUILD=1.
#   CARGO_BUILD_JOBS           rustc jobs of that build (default 3).
#   SOURCEGULL_CT_KEEP=1       keep the kind cluster after install (it is reused by the next run).
#   SOURCEGULL_CT_CNPG=1       also install the CloudNativePG operator, for ci values that use cnpg.
#   SOURCEGULL_CT_ALL=1        install even when the chart did not change against origin/main.
#   SOURCEGULL_CT_REBALANCE=1  also install with 3 repo replicas, seed repositories, upgrade to 4 and
#                              wait (REBALANCE_TIMEOUT s, default 600) until `repoRebalance` reports
#                              the moves done (scripts/test/helm-ct-rebalance.sh).
#
# The kind binary and the kubeconfig live in .ct/ (gitignored). The cluster is `sourcegull-ct` on the
# Docker network `kind`; it does not touch the sourcegull-k3s container of `scripts/dev k3s`.
set -euo pipefail

cd "$(dirname "$0")/../.."
ROOT=$(pwd)

KIND_VERSION=v0.29.0
CT_IMAGE=quay.io/helmpack/chart-testing:v3.12.0
HELM_IMAGE=alpine/helm:3.16.4
PYTHON_IMAGE=python:3.12-alpine
CNPG_MANIFEST=https://raw.githubusercontent.com/cloudnative-pg/cloudnative-pg/release-1.25/releases/cnpg-1.25.1.yaml
CLUSTER=sourcegull-ct
IMAGE=${SOURCEGULL_CT_IMAGE:-sourcegull/sourcegull:ct}
CHART=deploy/helm/sourcegull
# Images the chart's pods use besides Sourcegull; loaded into the node to avoid registry rate limits.
AUX_IMAGES=(postgres:18.6-bookworm busybox:1.37 curlimages/curl:8.11.1)

mkdir -p .ct

# Runs helm in Docker with the repository mounted at the same path.
helm_run() {
  docker run --rm -v "$ROOT:$ROOT" -w "$ROOT" "$HELM_IMAGE" "$@"
}

# The part of a chart version whose change marks a breaking release: major.minor while 0.x,
# major from 1.0.
breaking_part() {
  local major=${1%%.*} rest=${1#*.}
  if [[ $major == 0 ]]; then echo "0.${rest%%.*}"; else echo "$major"; fi
}

cmd_template() {
  local out=.ct/template.out f
  echo "== values.schema.json is generated from values.yaml"
  docker run --rm -v "$ROOT:$ROOT" -w "$ROOT" "$PYTHON_IMAGE" sh -c \
    "pip -q install --root-user-action=ignore pyyaml==6.0.2 >/dev/null 2>&1 && python -I scripts/helm-schema.py $CHART/values.yaml .ct/values.schema.json"
  diff -u "$CHART/values.schema.json" .ct/values.schema.json ||
    { echo "values.schema.json is stale: run scripts/helm-schema.py (see the chart README)"; exit 1; }
  echo "== helm lint / template"
  check() {
    echo "-- $*"
    helm_run lint --strict "$CHART" "$@" >"$out" 2>&1 || { cat "$out"; exit 1; }
    helm_run template sourcegull "$CHART" "$@" >"$out" 2>&1 || { cat "$out"; exit 1; }
  }
  check
  for f in "$CHART"/examples/*.yaml "$CHART"/ci/*-values.yaml; do
    check -f "$f"
  done
  check -f "$CHART/ci/bundled-values.yaml" -f deploy/helm/ct/kind-values.yaml
  check -f deploy/helm/ct/kind-values.yaml -f deploy/helm/ct/cnpg-values.yaml
  check -f deploy/helm/ct/options-values.yaml
  check --set ingress.enabled=true
  check --set postgresql.mode=cnpg \
    --set worker.autoscaling.keda.enabled=true --set postgresql.cnpg.backup.retentionPolicy=30d
  check --set postgresql.mode=external --set existingSecret=sourcegull-secrets
  check --set postgresql.mode=external --set externalDatabase.existingSecret=db --set worker.autoscaling.keda.enabled=true
  check --set worker.autoscaling.keda.enabled=true
  check --set worker.autoscaling.keda.enabled=true --set worker.autoscaling.keda.excludeQueues=null
  check --set worker.autoscaling.keda.enabled=true --set worker.autoscaling.keda.trigger=prometheus \
    --set worker.autoscaling.keda.prometheus.serverAddress=http://prometheus.monitoring.svc:9090
  check --set postgresql.mode=bundled --set networkPolicy.enabled=true \
    --set worker.autoscaling.keda.enabled=true --set worker.autoscaling.keda.trigger=prometheus \
    --set worker.autoscaling.keda.prometheus.serverAddress=https://prom.example.com \
    --set worker.autoscaling.keda.prometheus.threshold=10 \
    --set worker.autoscaling.keda.prometheus.authenticationRef=prom-auth \
    --set worker.autoscaling.keda.prometheus.authModes=bearer
  check --set worker.autoscaling.hpa.enabled=true
  check --set tests.smoke.enabled=true --set tests.smoke.credentialsSecret=sourcegull-smoke
  check -f scripts/test/codeintel/values.yaml --set image.tag=ci --set externalURL=http://sourcegull-k3s:30080
  check --set blobstore.mode=s3 --set blobstore.s3.bucket=sourcegull --set blobstore.s3.existingSecret=sourcegull-s3 \
    --set blobstore.s3.endpoint=http://minio:9000 --set blobstore.s3.caConfigMap=s3-ca
  check --set executor.enabled=true --set executor.jobs.namespace=sourcegull-jobs --set executor.jobs.runtimeClassName=gvisor \
    --set executor.jobs.nodeSelector.pool=executors --set executor.jobs.resources.limits.memory=4Gi
  check --set blobstore.persistence.size=250Gi --set blobstore.persistence.storageClass=fast
  check --set repo.persistence.enabled=false --set web.containerSecurityContext.enabled=false \
    --set web.podSecurityContext.enabled=false --set web.pdb.minAvailable= --set web.autoscaling.hpa.enabled=false
  check --set encryptionKeys.source=env
  check --set encryptionKeys.source=external --set 'web.command={/usr/bin/tini,--,/usr/local/bin/sourcegull}' \
    --set 'extraEnvVars[0].name=SOURCEGULL_KEYS' --set 'extraEnvVars[0].value=vault:secret/data/sourcegull#keys'
  check --set config.existingSecret=sourcegull-config --set 'config.existingSecretKeys={resources.json}'
  # `helm upgrade --reuse-values` (and ct's upgrade test) renders with the previous version's
  # values only: render the chart with main's values.yaml as its defaults.
  # A breaking version bump (minor while 0.x, major from 1.0; see the chart README) is exempt, as
  # in ct's upgrade test.
  local base old_ver new_ver
  base=$(git merge-base HEAD origin/main)
  old_ver=$(git show "$base:$CHART/Chart.yaml" | sed -n 's/^version: *//p')
  new_ver=$(sed -n 's/^version: *//p' "$CHART/Chart.yaml")
  if [[ $(breaking_part "$old_ver") != $(breaking_part "$new_ver") ]]; then
    echo "-- values.yaml of main (--reuse-values upgrade): skipped, breaking bump $old_ver -> $new_ver"
  else
    echo "-- values.yaml of main (--reuse-values upgrade)"
    rm -rf .ct/oldvals && mkdir -p .ct/oldvals && cp -r "$CHART" .ct/oldvals/sourcegull
    git show "$base:$CHART/values.yaml" >.ct/oldvals/sourcegull/values.yaml
    helm_run template sourcegull .ct/oldvals/sourcegull >"$out" 2>&1 || { cat "$out"; exit 1; }
  fi
  # Invalid combinations must fail.
  refuse() {
    echo "-- (must fail) $*"
    if helm_run template sourcegull "$CHART" "$@" >"$out" 2>&1; then
      echo "rendered, but should have failed: $*"; exit 1
    fi
  }
  refuse --set worker.autoscaling.hpa.enabled=true --set worker.autoscaling.keda.enabled=true
  refuse --set worker.autoscaling.keda.enabled=true --set worker.autoscaling.keda.minReplicas=0
  refuse --set worker.autoscaling.keda.enabled=true --set worker.autoscaling.keda.trigger=prometheus
  refuse --set worker.autoscaling.keda.enabled=true --set worker.autoscaling.keda.trigger=graphite
  refuse --set postgresql.mode=managed
  refuse --set encryptionKeys.source=vault
  refuse --set 'config.existingSecretKeys={site.json}'
  refuse --set postgresql.mode=external
  refuse --set web.podAntiAffinityPreset=always
  refuse --set web.bogus=1
  # Spot checks of the rendered output.
  helm_run template sourcegull "$CHART" --set postgresql.mode=cnpg \
    --set worker.autoscaling.keda.enabled=true >"$out"
  grep -q 'kind: Cluster' "$out" && grep -q 'name: sourcegull-pg-app' "$out" &&
    grep -q 'kind: ScaledObject' "$out" && ! grep -q 'name: sourcegull-postgresql$' "$out" ||
    { echo "cnpg/keda render check failed"; exit 1; }
  helm_run template sourcegull "$CHART" --set worker.autoscaling.keda.enabled=true \
    --set worker.autoscaling.keda.trigger=prometheus \
    --set worker.autoscaling.keda.prometheus.serverAddress=http://prometheus.monitoring.svc:9090 >"$out"
  grep -q 'type: prometheus' "$out" && grep -q 'threshold: "25"' "$out" &&
    grep -qF 'sourcegull_queue_jobs{state=~\"queued|processing\",queue!~\"codeintel_index_jobs|batch_spec_workspace_execution_jobs|repo_update_jobs\"}' "$out" &&
    ! grep -q 'kind: TriggerAuthentication' "$out" && ! grep -q 'type: postgresql' "$out" ||
    { echo "keda prometheus render check failed"; exit 1; }
  helm_run template sourcegull "$CHART" -f deploy/helm/ct/options-values.yaml >"$out"
  grep -q "name: 'sourcegull-extra'" "$out" && grep -q 'priorityClassName: "high"' "$out" &&
    grep -q 'storageClassName: ""' "$out" && grep -q 'secretName: sourcegull.example.org-tls' "$out" &&
    grep -q 'image: registry.example.com/sourcegull/sourcegull:' "$out" && grep -q 'vault.example/inject' "$out" ||
    { echo "options render check failed"; exit 1; }
  echo "template OK"
}

ct_run() {
  # The worktree's .git file points into the main repository's .git: mount that too, at the
  # same path, so ct's git calls (changed charts, the previous version) work.
  local common
  common=$(cd "$ROOT" && git rev-parse --path-format=absolute --git-common-dir)
  docker run --rm --network kind \
    -v "$ROOT:$ROOT" -v "$common:$common" -w "$ROOT" \
    -v "$ROOT/.ct/kubeconfig:/root/.kube/config:ro" \
    "$CT_IMAGE" sh -c "git config --global --add safe.directory '*' && $*"
}

cmd_lint() {
  echo "== ct lint"
  # Lints the charts changed against origin/main, including the version bump check.
  touch .ct/kubeconfig
  docker network inspect kind >/dev/null 2>&1 || docker network create kind >/dev/null
  ct_run ct lint --config deploy/helm/ct/ct.yaml
}

kind_bin() {
  local bin=.ct/kind-$KIND_VERSION
  if [ ! -x "$bin" ]; then
    echo "downloading kind $KIND_VERSION" >&2
    local url=https://github.com/kubernetes-sigs/kind/releases/download/$KIND_VERSION/kind-linux-amd64
    curl -fsSL -o "$bin.tmp" "$url"
    echo "$(curl -fsSL "$url.sha256sum" | cut -d' ' -f1)  $bin.tmp" | sha256sum -c - >/dev/null
    chmod +x "$bin.tmp" && mv "$bin.tmp" "$bin"
  fi
  echo "$ROOT/$bin"
}

cmd_install() {
  local kind
  kind=$(kind_bin)
  # Keep kind's context out of the user's ~/.kube/config.
  export KUBECONFIG=$ROOT/.ct/host-kubeconfig
  if ! "$kind" get clusters 2>/dev/null | grep -qx "$CLUSTER"; then
    echo "== creating kind cluster $CLUSTER"
    "$kind" create cluster --name "$CLUSTER" --wait 120s
  fi
  "$kind" get kubeconfig --name "$CLUSTER" --internal >.ct/kubeconfig

  if [ "${SOURCEGULL_CT_BUILD:-0}" = 1 ] || ! docker image inspect "$IMAGE" >/dev/null 2>&1; then
    echo "== building $IMAGE"
    scripts/image-build.sh -f deploy/docker/Dockerfile --load -t "$IMAGE" \
      --build-arg REVISION="$(git rev-parse HEAD)" .
  fi
  local img
  for img in "${AUX_IMAGES[@]}"; do
    docker image inspect "$img" >/dev/null 2>&1 || docker pull -q "$img" >/dev/null
  done
  # kind-values.yaml expects sourcegull/sourcegull:ct.
  [ "$IMAGE" = sourcegull/sourcegull:ct ] || docker tag "$IMAGE" sourcegull/sourcegull:ct
  echo "== loading images into $CLUSTER"
  # Not `kind load docker-image`: with Docker's containerd image store, `docker save` of a pulled
  # multi-platform image lists platforms whose layers are absent, and kind imports all platforms.
  for img in sourcegull/sourcegull:ct "${AUX_IMAGES[@]}"; do
    docker save "$img" | docker exec -i "$CLUSTER-control-plane" \
      ctr --namespace=k8s.io images import --platform linux/amd64 --digests --snapshotter=overlayfs - >/dev/null
  done

  if [ "${SOURCEGULL_CT_CNPG:-0}" = 1 ]; then
    echo "== installing the CloudNativePG operator"
    ct_run kubectl apply --server-side -f "$CNPG_MANIFEST"
    ct_run kubectl -n cnpg-system rollout status deploy/cnpg-controller-manager --timeout=180s
  fi

  echo "== ct install --upgrade"
  local start=$SECONDS
  # Only when the chart changed against origin/main, unless SOURCEGULL_CT_ALL=1.
  local which=""
  [ "${SOURCEGULL_CT_ALL:-0}" = 1 ] && which="--charts $CHART"
  ct_run ct install --config deploy/helm/ct/ct.yaml $which --upgrade \
    "--helm-extra-set-args '--values=deploy/helm/ct/kind-values.yaml'"
  echo "ct install took $((SECONDS - start)) s"

  if [ "${SOURCEGULL_CT_CNPG:-0}" = 1 ]; then
    # Not a ct case: the previous chart version has no cnpg mode to upgrade from.
    echo "== install with CloudNativePG + helm test"
    start=$SECONDS
    local ns=sourcegull-cnpg-$RANDOM
    ct_run "helm install sourcegull $CHART -n $ns --create-namespace \
        -f deploy/helm/ct/kind-values.yaml -f deploy/helm/ct/cnpg-values.yaml --wait --timeout 900s \
      && helm test sourcegull -n $ns --timeout 300s \
      && kubectl -n $ns get cluster,pods \
      ; rc=\$?; helm uninstall sourcegull -n $ns --wait; kubectl delete namespace $ns --wait=false; exit \$rc"
    echo "cnpg install took $((SECONDS - start)) s"
  fi

  if [ "${SOURCEGULL_CT_REBALANCE:-0}" = 1 ]; then
    # Not a ct case: ct installs each values file once, and this needs an upgrade with new values.
    start=$SECONDS
    ct_run "REBALANCE_TIMEOUT=${REBALANCE_TIMEOUT:-600} bash scripts/test/helm-ct-rebalance.sh \
      $CHART deploy/helm/ct/kind-values.yaml"
    echo "rebalance case took $((SECONDS - start)) s"
  fi

  if [ "${SOURCEGULL_CT_KEEP:-0}" != 1 ]; then
    "$kind" delete cluster --name "$CLUSTER"
  fi
}

case "${1:-}" in
  template) cmd_template ;;
  lint) cmd_lint ;;
  install) cmd_install ;;
  all) cmd_template; cmd_lint; cmd_install ;;
  down) KUBECONFIG=$ROOT/.ct/host-kubeconfig "$(kind_bin)" delete cluster --name "$CLUSTER" ;;
  *) sed -n '2,27p' "$0" | sed 's/^# \{0,1\}//'; exit 2 ;;
esac
