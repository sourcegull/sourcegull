# Sourcegull Helm chart

Deploys Sourcegull on Kubernetes. Every value is documented in
[`values.yaml`](values.yaml) and validated by [`values.schema.json`](values.schema.json), which
is generated from it: after changing values.yaml run
`docker run --rm -v "$PWD:$PWD" -w "$PWD" python:3.12-alpine sh -c 'pip -q install pyyaml==6.0.2 && python -I scripts/helm-schema.py deploy/helm/sourcegull/values.yaml deploy/helm/sourcegull/values.schema.json'`
(enums, patterns and minimums are in its `RULES`);
[`SIZING.md`](SIZING.md) and [`examples/`](examples/) give small, medium and large presets.

Each release publishes the chart to `oci://ghcr.io/sourcegull/charts/sourcegull` (Helm 3.8 or
later); the [release notes](https://github.com/sourcegull/sourcegull/releases) give its version.
The presets in `examples/` come with the chart (`helm pull --untar` it to use them):

```sh
helm pull oci://ghcr.io/sourcegull/charts/sourcegull --untar
helm install sourcegull oci://ghcr.io/sourcegull/charts/sourcegull \
  -f sourcegull/examples/small.yaml --set externalURL=https://sourcegull.example.com
helm test sourcegull
```

From a checkout of this repository, use `deploy/helm/sourcegull` as the chart instead.

With no values at all the chart installs a working evaluation instance: the bundled database,
generated encryption keys (back up the Secret, see the install notes) and the builtin blob store.

## Database (`postgresql.mode`)

| Mode | What runs | Database URL |
|---|---|---|
| `bundled` (default) | one PostgreSQL StatefulSet (no HA, no backups) | built from the generated password Secret (or `postgresql.auth.existingSecret`) |
| `cnpg` | a CloudNativePG `Cluster` (needs the CloudNativePG operator) | the `uri` key of the operator's `<fullname>-pg-app` Secret |
| `external` | nothing; bring PostgreSQL 18 | `database-url` in `existingSecret`, or `externalDatabase.existingSecret` / `existingSecretUrlKey` |

With `external`, migrations run in a pre-install/pre-upgrade hook Job; with `bundled` and `cnpg`
they run in the init containers of the Sourcegull pods, which first wait for the database.

The bundled database runs PostgreSQL 18, its data directory `pgdata` on the `data` volume. With
`cnpg`, the chart leaves the image to `postgresql.cnpg.imageName` (empty: the operator's
default). With `external`, Sourcegull is tested on PostgreSQL 18.

## Encryption keys (`encryptionKeys.source`)

Every Sourcegull container needs the same key-encryption keys, from the `encryption-keys` entry of
the app Secret (`existingSecret`, or the Secret the chart generates when it is empty):

| Source | The containers get | Rotation |
|---|---|---|
| `file` (default) | the entry mounted as `SOURCEGULL_KEYS_FILE` | update the Secret, wait for the kubelet to refresh the mounted file (up to about a minute), then run `sourcegull keys rotate` in a pod; the other pods re-read the file |
| `env` | `SOURCEGULL_KEYS` from the entry (`secretKeyRef`; no key in the pod spec) | update the Secret, then `kubectl rollout restart` every Sourcegull workload |
| `external` | neither: set `SOURCEGULL_KEYS` yourself | change the source, then restart every Sourcegull workload |

To rotate, append the new key to the entry (`k1:<old>` and `k2:<new>` on separate lines or
comma-separated, the last one current). Updating the Secret alone does not rotate: the rotation
runs when a process starts or `sourcegull keys rotate` runs. When re-encryption reaches 100% on
**Site admin → Migrations**, remove the old key the same way (with `env` or `external`, that is a
second restart). See [configuration](../../../docs/configuration.md#encryption-keys).

Generate a key with `printf 'k1:%s\n' "$(openssl rand -base64 32)"` (see
[configuration](../../../docs/configuration.md#encryption-keys)). With `external`, a
secret-injecting webhook can provide the value:

```yaml
encryptionKeys:
  source: external
extraEnvVars:
  - name: SOURCEGULL_KEYS
    value: vault:secret/data/sourcegull#keys
# The webhook wraps the container command, which it reads from the pod spec; likewise for
# worker, repo, blobstore and migrate.
web:
  command: ["/usr/bin/tini", "--", "/usr/local/bin/sourcegull"]
  podAnnotations: { ... }   # the webhook's annotations
```

Sourcegull reads `SOURCEGULL_KEYS` once and removes it from its environment, so the processes it starts
do not inherit it; a value still starting with `vault:` fails startup with an error saying the
injector did not run.

## Configuration as code

`config.siteConfig`, `config.globalSettings` and `config.resources` hold the site configuration,
the global settings and the [resources file](../../../docs/configuration.md#resources-file)
(connections, search contexts, roles, site admins, outbound webhooks, executor secrets, code
intelligence policies, feature flags) as YAML. Each one that is set is rendered as JSON into the
`<fullname>-config` Secret, mounted on every Sourcegull pod, and is read-only in the UI. Changes apply
within about a minute of `helm upgrade`, without a restart. Secrets stay out of the values file:
reference them with `${env:NAME}` and pass the variables with `extraEnvVars` (or a whole Secret
with `extraEnvVarsSecret`), or mount files with each component's `extraVolumes` and
`extraVolumeMounts` and use `${file:/path}`.

```yaml
extraEnvVars:
  - name: GITLAB_TOKEN
    valueFrom: { secretKeyRef: { name: gitlab, key: token } }
  - name: OIDC_CLIENT_SECRET
    valueFrom: { secretKeyRef: { name: oidc, key: client-secret } }
config:
  siteConfig:
    auth.providers:
      - type: openidconnect
        displayName: SSO
        issuer: https://sso.example.com
        clientID: sourcegull
        clientSecret: "${env:OIDC_CLIENT_SECRET}"
  globalSettings:
    search.defaultPatternType: keyword
  resources:
    siteAdmins: [admin@example.com]
    connections:
      - key: gitlab-main
        kind: gitlab
        displayName: GitLab
        config:
          url: https://gitlab.example.com
          token: "${env:GITLAB_TOKEN}"
          projectQuery: [groups/acme/projects]
```

With `siteAdmins` declared, the first SSO sign-in of a listed (verified) address is site admin
and no other first user is, so an install needs no clicks. To render the files elsewhere (for
example from a secrets operator), set `config.existingSecret` to a Secret with any of the keys
`site-config.json`, `global-settings.json` and `resources.json`, and list them in
`config.existingSecretKeys`. Check the documents in CI with `sourcegull config check`.

## Worker autoscaling

`worker.autoscaling.keda.enabled=true` renders a KEDA `ScaledObject` (and `TriggerAuthentication`)
whose `postgresql` trigger counts the jobs queued and due plus the jobs running across the
worker's pgqueue tables. `worker.autoscaling.hpa.enabled=true` is a CPU HPA for clusters without KEDA. They are
exclusive, and neither scales below one replica (the worker runs leader-elected singletons).

With `worker.autoscaling.keda.trigger=prometheus` the `ScaledObject` uses KEDA's `prometheus`
trigger instead, and KEDA needs no access to the database (no `TriggerAuthentication` is rendered
and no NetworkPolicy rule lets the KEDA operator into the bundled database). It queries
`worker.autoscaling.keda.prometheus.serverAddress` (required) for

```promql
sum(max by (queue, state) (sourcegull_queue_jobs{state=~"queued|processing",queue!~"<excludeQueues>"}))
```

and runs ceil(value / `prometheus.threshold`) workers (`threshold` defaults to
`targetJobsPerReplica`). Every worker process exports the gauge from Postgres every 30 s, so
Prometheus must scrape the worker pods (`metrics.serviceMonitor.enabled=true`, or your own scrape config).
Differences from the SQL trigger: the value lags by up to the collection plus scrape interval,
and `queued` includes jobs waiting out a retry delay. `prometheus.query` replaces the PromQL;
`prometheus.authenticationRef` (an existing `TriggerAuthentication`) and `prometheus.authModes`
configure authentication to Prometheus.

```yaml
worker:
  autoscaling:
    keda:
      enabled: true
      trigger: prometheus
      prometheus:
        serverAddress: http://prometheus-operated.monitoring.svc:9090
        threshold: 25
```

## Pod settings

`web`, `worker`, `repo`, `blobstore` (the builtin blob StatefulSet) and `executor` take the same
settings, documented once under `web` in `values.yaml`; `migrate` and `postgresql` take the
subset that applies to them.

| Settings | Values |
|---|---|
| Replicas and updates | `replicaCount`, `updateStrategy`, `autoscaling.hpa` (web, worker), `pdb.create` / `minAvailable` / `maxUnavailable` |
| Container | `command`, `args`, `resources`, `lifecycleHooks` (default: a 5 s preStop pause on web, repo and blob), `terminationGracePeriodSeconds` |
| Probes | `startupProbe`, `livenessProbe`, `readinessProbe` (`enabled` and timings), `customStartupProbe` and so on to replace one |
| Security | `podSecurityContext`, `containerSecurityContext` (each with `enabled`) |
| Environment | `extraEnvVars`, `extraEnvVarsCM`, `extraEnvVarsSecret`, after the top-level ones of the same names (which apply to every Sourcegull container but the executor) |
| Extra objects | `extraVolumes`, `extraVolumeMounts`, `sidecars`, `initContainers`, `emptyDirSizeLimit` |
| Scheduling | `podAffinityPreset`, `podAntiAffinityPreset` (`soft` by default), `nodeAffinityPreset`, or `affinity` instead; `nodeSelector`, `tolerations`, `topologySpreadConstraints`, `priorityClassName`, `schedulerName`, `hostAliases` |
| Metadata | `podLabels`, `podAnnotations`; top-level `commonLabels` and `commonAnnotations` go on every resource |
| Storage (repo, blob, postgresql) | `persistence` (`size`, `storageClass`, `accessModes`, `annotations`, `labels`; `enabled: false` uses an emptyDir on repo and blob), `persistentVolumeClaimRetentionPolicy` |

`storageClass` falls back to `global.defaultStorageClass`; `"-"` sets an empty class (no
dynamic provisioning). `global.imageRegistry` overrides every image's `registry`. Extra lists and
maps go through `tpl`, so they can use `{{ .Release.Name }}` and the like; `extraDeploy` renders
whole extra manifests.

## Tests

`helm test` always runs `test-readyz`. With `tests.smoke.enabled=true` it also signs in with a
built-in account, runs a GraphQL query and a streaming search (`test-smoke`). The account comes
from `tests.smoke.credentialsSecret` (keys `username`, `password`) or a generated Secret;
`tests.smoke.signUp=true` signs it up first, which on a fresh install makes it the site admin, so
use that only on throwaway installs.

## Versions

- `version` in `Chart.yaml` follows SemVer and is bumped by **every** change to the chart
  (`ct lint` fails otherwise). While the chart is 0.x, chart-testing treats a minor bump as
  breaking, so: patch (0.1.x) for fixes and for new values or behaviour that keep existing values
  files and installs working; minor (0.x.0) when an existing values file or install needs changes
  (ct then skips the upgrade test, so such a bump needs an upgrade note in the table below).
  From 1.0: patch for fixes, minor for compatible additions, major for breaking changes.
- `appVersion` is the Sourcegull release the chart deploys by default (`image.tag` defaults to it).
- Upgrades are tested from the previous chart version: `ct install --upgrade` installs the chart as
  it is on `main`, upgrades it to the working tree's chart and runs `helm test`.

| Chart | Changes |
|---|---|
| 0.1.0 | First release |

## Testing the chart

`scripts/test/helm-ct.sh` runs everything in Docker:

- `template`: checks that `values.schema.json` is current, then `helm lint --strict` and
  `helm template` for the defaults, each preset, each `ci/*-values.yaml`,
  [`../ct/options-values.yaml`](../ct/options-values.yaml) (every option at once) and an option
  matrix, plus combinations that must be refused;
- `lint`: chart-testing `ct lint` (yamllint, Chart.yaml schema, version bump against `main`);
- `install`: a kind cluster, the Sourcegull image built from `deploy/docker/Dockerfile` and loaded into
  it, then `ct install --upgrade` with [`../ct/kind-values.yaml`](../ct/kind-values.yaml) layered
  on every install. CI runs the same in `.github/workflows/helm.yml`.
