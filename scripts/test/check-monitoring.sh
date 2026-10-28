#!/usr/bin/env bash
# Checks the generated monitoring files (docs/observability/) with the real
# tools, in Docker (only Docker is needed; a local `helm` is used when present):
#
#   - `promtool check rules` on the generated alerting rules
#   - `promtool check config` on the Compose Prometheus config (with the rules file in place)
#   - `helm lint --strict` and `helm template` of the chart with the ServiceMonitor,
#     PrometheusRule and dashboard ConfigMaps on, and `promtool check rules` on the rendered
#     PrometheusRule's groups
#
# That the files are current is checked by `cargo test -p sourcegull-monitoring --test integration generated::`.
set -euo pipefail
cd "$(dirname "$0")/../.."

PROMETHEUS_IMAGE=prom/prometheus:v3.5.0
HELM_IMAGE=alpine/helm:3.16.4

promtool() {
  # $1: a directory mounted at /w; the rest: promtool arguments.
  local dir=$1
  shift
  docker run --rm -v "$dir:/w:ro" --entrypoint promtool "$PROMETHEUS_IMAGE" "$@"
}

helm_cmd() {
  if command -v helm >/dev/null; then
    helm "$@"
  else
    docker run --rm -v "$PWD/deploy/helm/sourcegull:/chart:ro" "$HELM_IMAGE" "${@/deploy\/helm\/sourcegull//chart}"
  fi
}

echo "== promtool check rules (Compose copy)"
promtool "$PWD/deploy/docker/prometheus" check rules /w/sourcegull-alerts.yml

echo "== promtool check config (Compose)"
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
# The config names /etc/prometheus/sourcegull-alerts.yml; check it with the rules file next to it.
sed 's#/etc/prometheus/sourcegull-alerts.yml#/w/sourcegull-alerts.yml#' deploy/docker/prometheus/prometheus.yml >"$tmp/prometheus.yml"
cp deploy/docker/prometheus/sourcegull-alerts.yml "$tmp/"
chmod -R a+rX "$tmp"
promtool "$tmp" check config /w/prometheus.yml

echo "== helm lint and template with monitoring on"
flags=(--set existingSecret=sourcegull-secrets
  --set metrics.serviceMonitor.enabled=true --set metrics.prometheusRule.enabled=true
  --set metrics.grafanaDashboards.enabled=true --set metrics.serviceMonitor.labels.release=prometheus
  --set metrics.prometheusRule.labels.release=prometheus)
helm_cmd lint --strict deploy/helm/sourcegull "${flags[@]}"
helm_cmd lint --strict deploy/helm/sourcegull "${flags[@]}" \
  --set 'metrics.blobScrapeFrom[0].namespaceSelector.matchLabels.name=monitoring'
helm_cmd template sourcegull deploy/helm/sourcegull "${flags[@]}" >"$tmp/rendered.yaml"
for kind in ServiceMonitor PrometheusRule; do
  grep -q "^kind: $kind\$" "$tmp/rendered.yaml" || { echo "no $kind rendered" >&2; exit 1; }
done
dashboards=$(ls deploy/helm/sourcegull/files/monitoring/dashboards/*.json | wc -l)
rendered=$(grep -c '^  sourcegull-.*\.json: |-$' "$tmp/rendered.yaml" || true)
[ "$dashboards" = "$rendered" ] || { echo "expected $dashboards dashboard ConfigMaps, got $rendered" >&2; exit 1; }

# The PrometheusRule's spec is a rules file: cut it out (from `spec:` up to the next document).
awk '/^kind: PrometheusRule$/{p=1} p&&/^---/{p=0} p&&s{sub(/^  /,""); print} p&&/^spec:$/{s=1}' \
  "$tmp/rendered.yaml" >"$tmp/prometheusrule.yml"
promtool "$tmp" check rules /w/prometheusrule.yml

echo "monitoring checks passed"
