# Monitoring

Every Sourcegull process serves Prometheus metrics at `/metrics` on its internal listener
(`:6060`). Sourcegull ships alerting rules and Grafana dashboards for them, generated from
`crates/monitoring`. Load them into your own Prometheus and Grafana:

- [alerts.md](alerts.md): every alert, with next steps. Rules file:
  `prometheus/sourcegull-alerts.yml` in the Compose bundle.
- [dashboards.md](dashboards.md): one dashboard per service. JSON files:
  `grafana/dashboards/*.json` in the Compose bundle.

The dashboards and alerts select series by a `role` label: `web`, `worker`, `repo` or `blob`
for a process running that role, or `all` for one running every role (`sourcegull supervise`,
`--roles=all`). Set it in the scrape config.

## Helm

The chart creates the ServiceMonitor (`metrics.serviceMonitor.enabled=true`, sets `role`),
the PrometheusRule (`metrics.prometheusRule.enabled=true`) and the dashboard ConfigMaps for
the Grafana sidecar (`metrics.grafanaDashboards.enabled=true`). See the
[chart README](../../deploy/helm/sourcegull/README.md).

## Docker Compose

[`prometheus/prometheus.yml`](../../deploy/docker/prometheus/prometheus.yml) in the bundle is
an example scrape config for both Compose files.

The internal listener is not published: it also serves the blob API of a process running the
`blob` role (`compose.yaml`, and the `blob` service of `compose.production.yaml`), which has
no authentication. A Prometheus in a container on the same host can join the Compose network
and scrape `sourcegull:6060` (or `web:6060`, … in the production layout):

```yaml
# In your Prometheus' Compose file.
services:
  prometheus:
    networks: [default, sourcegull]
networks:
  sourcegull:
    name: sourcegull_default
    external: true
```

For a Prometheus elsewhere, publish the port on an address that only it can reach, with a
`compose.override.yaml` next to `compose.yaml` (Compose reads it with the default file names):

```yaml
services:
  sourcegull:
    ports:
      - "10.0.0.5:6060:6060"   # a private address of this host
```

With `compose.production.yaml`, put the override in its own file and add it with `-f` (or to
`COMPOSE_FILE` in `.env`, separated by `:`), and publish each service on its own host port.

Then:

1. Add the scrape job with the `role` label (as in the example), and
   `prometheus/sourcegull-alerts.yml` under `rule_files`.
2. In Grafana, import each `grafana/dashboards/*.json` (Dashboards → New → Import) and pick
   your Prometheus data source. Or provision the directory with a
   [dashboard provider](https://grafana.com/docs/grafana/latest/administration/provisioning/#dashboards).
