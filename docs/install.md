# Install

Sourcegull ships as one image, `ghcr.io/sourcegull/sourcegull`, which holds the `sourcegull` server
binary and the `gull` command-line client, for linux/amd64 (for arm64,
[build from source](#building-from-source)). There are two ways to
run it:

| Option | For | Database |
|---|---|---|
| [Docker Compose](#docker-compose) | trying Sourcegull, one host | PostgreSQL service in the Compose file |
| [Kubernetes (Helm)](#kubernetes-helm) | larger installs, several repo nodes | external, bundled or CloudNativePG |

Every option needs only PostgreSQL 18; there is no Redis or other service to run. Install from
a [release](https://github.com/sourcegull/sourcegull/releases): each one publishes the images,
the Helm chart and a Compose bundle, and its notes give the versions.

## Release artifacts

Each release publishes, signed by digest (images with an SBOM and build provenance):

| Artifact | Contents |
|---|---|
| `ghcr.io/sourcegull/sourcegull` | the server and `gull`; what Compose and the Helm chart run |
| `ghcr.io/sourcegull/gull` | only `gull` and git, for CI jobs ([SCIP uploads](code-navigation.md)) |
| `oci://ghcr.io/sourcegull/charts/sourcegull` | the [Helm chart](#kubernetes-helm), versioned on its own |
| `sourcegull-compose.tar.gz` (release asset) | the [Docker Compose](#docker-compose) files: `compose.yaml`, `.env.example`, configuration and monitoring files |

Image tags: `X.Y.Z`, `X.Y` (the newest patch release) and `latest`. The Compose bundle and the
chart default to the image of their release, so neither needs a build. The release notes show how to check a
signature with `cosign verify`.

### Building from source

To run a change of your own, build the image from a checkout of this repository and point
Compose (`SOURCEGULL_IMAGE`, `SOURCEGULL_VERSION` in `.env`) or the chart (`image.registry`,
`image.repository`, `image.tag`) at it:

```sh
docker buildx build -f deploy/docker/Dockerfile -t registry.example.com/sourcegull/sourcegull:dev --load .
```

The build compiles the web UI and embeds it in the binary. `--platform linux/arm64` builds an arm64
image (cross-compiled, no emulation needed for the Rust build); `--target cli` builds the `gull` image.

## Docker Compose

The Compose bundle runs PostgreSQL, a one-shot `migrate` service, `web`, `worker`, one repo node
(`repo-0`) and a `blob` store, each as its own container with a read-only root file system.
Download it from the latest release (the same files are in `deploy/docker/` of a checkout):

```sh
curl -fsSL https://github.com/sourcegull/sourcegull/releases/latest/download/sourcegull-compose.tar.gz | tar -xz
cd sourcegull-compose
cp .env.example .env            # then change every password
mkdir -p secrets && printf 'k1:%s\n' "$(openssl rand -base64 32)" > secrets/encryption-keys
chmod 0644 secrets/encryption-keys   # read by UID 10001 in the containers
docker compose up -d            # pulls ghcr.io/sourcegull/sourcegull
```

Back up `secrets/encryption-keys` together with the database: without it, the stored code host
tokens and other secrets cannot be decrypted. The keys can also be passed in `SOURCEGULL_KEYS`
instead of a file ([configuration](configuration.md#keys-in-the-environment)). `.env.example`
documents every setting, including memory limits and Postgres tuning.

To configure the instance from files instead of the UI, put the site configuration, global
settings and resources files in `config/` and name them in `.env`
([configuration as code](configuration.md#configuration-as-code));
`config/resources.example.jsonc` is a starting point.

Optional profiles:

- `docker compose --profile executors up -d` starts an [executor](architecture.md#executors) for
  auto-indexing and server-side bulk changes. It uses the host's Docker socket; set
  `SOURCEGULL_EXECUTOR_TOKEN` in `.env` to the same value as `executors.accessToken` in the site
  configuration, and `DOCKER_GID` to the group that owns `/var/run/docker.sock`.
- `docker compose --profile monitoring up -d` starts Prometheus with Sourcegull's alerting rules and
  Grafana with its dashboards on <http://localhost:3000> (see [observability](observability/)).
- `docker compose --profile minio up -d` starts MinIO, for [S3 blob storage](#s3-blob-storage).

To add repo nodes, copy the `repo-0` service and its volume as `repo-1`, … and list every node in
`SOURCEGULL_REPO_NODES` (in `.env`), then `docker compose up -d`. Repositories move to new nodes in
the background without downtime ([rebalancing](architecture.md#repo-nodes-and-rebalancing)).

Upgrades (pull, migrate, up), multi-version upgrades, backups and key rotation are described in
[upgrade.md](../deploy/docker/upgrade.md), which the bundle includes.

## Kubernetes (Helm)

The [chart](../deploy/helm/sourcegull/README.md), published as
`oci://ghcr.io/sourcegull/charts/sourcegull` (Helm 3.8 or later), deploys `web`, the repo
nodes as a StatefulSet, `worker`, the blob store, optional executors (Kubernetes runtime: one
Job per auto-indexing job, one per bulk change step), a migration hook and optional monitoring
objects. It runs `ghcr.io/sourcegull/sourcegull` at the chart's `appVersion` (`0.1.1`); set
`image.registry` and `image.repository` for a mirror or an [image of your own](#building-from-source).
With no other values the chart runs the bundled database and generates the encryption keys,
which is enough to evaluate Sourcegull. The size presets come with the chart:

```sh
helm pull oci://ghcr.io/sourcegull/charts/sourcegull --untar   # the chart, with examples/
helm install sourcegull oci://ghcr.io/sourcegull/charts/sourcegull \
  -f sourcegull/examples/small.yaml --set externalURL=https://sourcegull.example.com
helm test sourcegull
```

`--version` pins a chart version (the release notes give it); from a checkout,
`deploy/helm/sourcegull` works as the chart too.

For production, use an external PostgreSQL 18 and your own Secret:

```sh
kubectl create secret generic sourcegull-secrets \
  --from-literal=database-url='postgres://sourcegull:...@db.example.com:5432/sourcegull' \
  --from-literal=encryption-keys="k1:$(openssl rand -base64 32)"
helm install sourcegull oci://ghcr.io/sourcegull/charts/sourcegull \
  -f sourcegull/examples/large.yaml --set externalURL=https://sourcegull.example.com
```

- Database: `postgresql.mode` is `bundled` (default; one PostgreSQL StatefulSet, for evaluation
  and small installs), `cnpg` (a CloudNativePG cluster) or `external` (with `existingSecret` or
  `externalDatabase.existingSecret`).
- Sizing: [SIZING.md](../deploy/helm/sourcegull/SIZING.md) and the small, medium and large presets
  in `examples/`.
- Workers can autoscale on job queue depth with KEDA (`worker.autoscaling.keda.enabled`).
- Singleton background tasks are leader-elected with Kubernetes `Lease` objects (the chart
  grants the RBAC).
- `metrics.*` renders a ServiceMonitor, a PrometheusRule and Grafana dashboard ConfigMaps.
- `config.siteConfig`, `config.globalSettings` and `config.resources` configure the instance
  from the values file: sign-in, code host connections, site admins and the rest, with secrets
  referenced from `extraEnvVars` ([chart README](../deploy/helm/sourcegull/README.md#configuration-as-code)).
  With these set, an install clones and indexes its repositories with no clicks.
- `encryptionKeys.source` passes the keys as a mounted file (default), as `SOURCEGULL_KEYS` from the
  Secret, or leaves `SOURCEGULL_KEYS` to a secret injector.

- Every component takes the same pod settings: `command`, probes, security contexts, scheduling
  presets, `priorityClassName`, `extraEnvVars`, `extraVolumes`, `sidecars`, `initContainers` and
  the rest ([chart README](../deploy/helm/sourcegull/README.md#pod-settings)).

Every value is documented in `values.yaml` and validated by `values.schema.json`.

## S3 blob storage

The blob store holds SCIP uploads, code graph files and search job results. By default it is
the `blob` role's directory (`<data dir>/blobs`). Any S3-compatible bucket (MinIO, Garage,
SeaweedFS, Ceph RGW, Amazon S3) can be used instead:

```sh
SOURCEGULL_BLOB_STORE=s3://sourcegull-blobs/prod     # s3://<bucket>[/<prefix>]
SOURCEGULL_S3_ENDPOINT=http://minio:9000         # empty: Amazon S3
SOURCEGULL_S3_REGION=us-east-1
SOURCEGULL_S3_ACCESS_KEY_ID=...
SOURCEGULL_S3_SECRET_ACCESS_KEY=...
```

Other settings: `SOURCEGULL_S3_PATH_STYLE` (default `true`; `false` for virtual-hosted buckets),
`SOURCEGULL_S3_SESSION_TOKEN`, `SOURCEGULL_S3_CA_CERT` (PEM file of a private CA),
`SOURCEGULL_S3_INSECURE_SKIP_VERIFY`, `SOURCEGULL_S3_PART_SIZE` (multipart part size, default 8 MiB) and
`SOURCEGULL_S3_CREATE_BUCKET` (create the bucket at startup). The standard `AWS_*` variables are read
when the `SOURCEGULL_S3_*` ones are not set. Credentials are never taken from the URL.

- Compose: set these in `.env` and stop the blob service with
  `docker compose up -d --scale blob=0`.
- Helm: `blobstore.mode=s3` with `blobstore.s3.*` (bucket, endpoint, and a Secret with the key
  pair in `blobstore.s3.existingSecret`).

## First sign-in

Open the external URL (`http://localhost:7080` by default). The first account to sign up
becomes the site admin; after that, sign-up follows the site configuration
(`"allowSignup": false` on the built-in provider by default, so the admin creates accounts or
users [request access](permissions.md#access-requests)). With `siteAdmins` in the
[resources file](configuration.md#resources-file), the users with those verified emails are site
admins instead, and no other first user is.

Set `externalURL` in **Site admin → Site configuration** (or `SOURCEGULL_EXTERNAL_URL`) before you
configure OAuth sign-in or webhooks: callback and webhook URLs are built from it.

## Connecting GitLab

Add a connection under **Site admin → Code host connections**. The **Settings** tab is a form:

- **GitLab URL** and **Token**. *How to create the token* links to the token page of your
  GitLab with the name and scope filled in: `read_api` for search, navigation and
  permissions, `api` if the connection is also used for bulk changes or webhook registration.
  A group or project access token works too.
- **Projects to mirror**: every project the token's user is a member of, selected groups
  (with subgroups) and projects, or custom GitLab API queries. Paths and URLs can be pasted.
  **Exclusions** skip projects by path, by regular expression or when empty.
- **Repository permissions**: every Sourcegull user, or GitLab permissions (see
  [permissions](permissions.md)).
- **Advanced**: clone over HTTPS or SSH, request budget, repository name pattern, internal
  projects as public, webhook auto-registration, TLS certificate.

**Test connection** checks the URL and token before you save and shows the GitLab user the
token belongs to and its scopes. **Edit as JSON** shows the same configuration as JSON with
comments, validated as you type; the form changes only the values it touches, and settings it
doesn't show (listed under the form) stay as they are.

```jsonc
{
  "url": "https://gitlab.example.com",
  // read_api is enough for search and navigation; bulk changes need api.
  "token": "glpat-...",
  // Which projects to mirror: GitLab API list paths, explicit projects, or both.
  "projectQuery": ["groups/acme/projects?include_subgroups=true"],
  "projects": [{ "name": "platform/api" }],
  "exclude": [{ "pattern": "^acme/archive/" }],
  // Apply GitLab permissions to Sourcegull users (see permissions.md).
  "authorization": { "identityProvider": { "type": "oauth" } }
}
```

### GitHub App authentication

Instead of `token`, a connection can authenticate as a GitHub App installation (set exactly one
of `token` and `githubApp`). Installation tokens are not tied to a user account, have their own,
higher rate limits, and are what repository permissions need to list collaborators without an
admin's personal token.

1. Register a GitHub App (**Settings → Developer settings → GitHub Apps → New GitHub App**, on
   your account or the organization). It needs no callback or webhook URL. Permissions:
   repository **Contents** and **Metadata** read-only; for [repository
   permissions](permissions.md#github-repository-permissions), also organization **Members**
   read-only.
2. Note the **App ID** and generate a **private key** (a `.pem` file).
3. Install the App on the organization or account, for all repositories or selected ones. The
   **installation ID** is the number at the end of the installation's settings URL
   (`…/settings/installations/12345678`).

```jsonc
{
  "url": "https://github.com",
  "githubApp": {
    "appID": 123456,
    "installationID": 12345678,
    // The PEM text, or a file reference in a configuration file.
    "privateKey": "${file:/run/secrets/github-app.pem}"
  },
  // With a GitHub App, "affiliated" lists every repository of the installation.
  "repositoryQuery": ["affiliated"],
  "rateLimit": { "requestsPerHour": 15000 }
}
```

Sourcegull signs a short-lived JWT with the private key and exchanges it for an installation token
(valid for an hour). The token is used for every API request and for HTTPS clones and fetches
(as user `x-access-token`), cached per connection and replaced five minutes before it expires
or when GitHub rejects it; one request mints while others wait for its token. The private key
is redacted like the token wherever the configuration is shown, and `${file:…}` and `${env:…}`
references work for it in [configuration files](configuration.md#configuration-as-code).
**Test connection** shows the App and installation instead of a user. An installation's rate
limit is at least 5,000 requests per hour (15,000 on GitHub Enterprise Cloud); raise
`rateLimit.requestsPerHour` to match.

Other keys: `repositoryPathPattern` (default `{host}/{pathWithNamespace}`),
`nameTransformations`, `gitURLType` (`http` or `ssh`, with `SOURCEGULL_REPO_SSH_KEY` on the repo
nodes), `certificate` (PEM of an internal CA), `rateLimit` (`requestsPerHour`, shared by every
Sourcegull process), `markInternalReposAsPublic`, `webhooks`, `webhooks.autoRegister` and
`systemHook`.

### OAuth token

Instead of a personal access token, a connection can use a GitLab OAuth access token issued
by the OAuth application of a GitLab entry in `auth.providers` for the same URL (see
[permissions](permissions.md#gitlab-oauth)):

```jsonc
{
  "url": "https://gitlab.example.com",
  "token": "<access token>",
  "token.type": "oauth",
  "token.oauth.refresh": "<refresh token>",
  // When the access token expires, in Unix seconds (created_at + expires_in).
  "token.oauth.expiry": 1767225600
}
```

GitLab OAuth access tokens expire after two hours. Sourcegull refreshes the token five minutes
before `token.oauth.expiry`, and when GitLab answers 401, then retries the request once. The
refresh uses the client ID and secret of the matching `auth.providers` entry (each entry for
the URL is tried). GitLab replaces the refresh token on every refresh, so Sourcegull writes the new
`token`, `token.oauth.refresh` and `token.oauth.expiry` back into the connection (other
settings, comments and the sync state are kept) and every Sourcegull process picks them up. If
GitLab rejects the refresh token (revoked, or already used), the connection's syncs fail with
an error saying so; Sourcegull does not retry until you set a new `token` and
`token.oauth.refresh`.

Sourcegull lists the projects, then clones and indexes them; progress shows in the status icon in
the top bar and under **Site admin → Repositories**. Repositories are fetched on a schedule;
for updates within seconds, add a GitLab webhook (push and tag events, or a system hook on
self-managed GitLab) pointing at the URL shown on the connection page, with one of the
connection's `webhooks` secrets as its secret token.

The connection page shows the webhook secret only masked (`…` and its last 4 characters), as
the API does. **Generate secret** (or **Regenerate secret**) replaces the connection's
`webhooks` secrets with a new random one and shows it once, with a copy button; set it on the
code host's webhook then, because it is not shown again. Webhooks sent with the old secret are
rejected from then on. This applies to GitHub and Gitea connections too.

### Registering webhooks automatically

With `"webhooks.autoRegister": true`, Sourcegull creates the webhook on every project the
connection syncs, so you don't add them by hand:

```jsonc
{
  "url": "https://gitlab.example.com",
  "token": "glpat-…",
  "projectQuery": ["groups/acme/projects?include_subgroups=true"],
  "webhooks": [{ "secret": "…" }],   // or "Generate secret" on the connection page
  "webhooks.autoRegister": true
}
```

- The token needs the `api` scope and the Maintainer role (or higher) on the projects. Projects
  where GitLab refuses (403) are listed as warnings in the connection's sync history; the rest
  of the sync is unaffected.
- `externalURL` must be set in the site configuration to an address GitLab can reach. When it
  is unset or a local address (`localhost`, `127.0.0.1`), nothing is registered and the sync
  shows a warning.
- The hook points at the connection's webhook URL (shown on the connection page) with push,
  tag push, merge request and pipeline events, and the first `webhooks` secret as its secret
  token. Existing hooks to that URL are reused: one with other events, another host (after
  `externalURL` changed) or an old secret is updated, and duplicates are deleted.
- A full sync (hourly) checks every project's hooks; incremental syncs set up new projects.
  Each check is one API request per project, drawn from the connection's `rateLimit`.
- Regenerating the webhook secret starts a full sync that updates every hook with the new
  secret. Projects that leave the connection have their hook removed.

For precise code navigation, see [code navigation](code-navigation.md); for sign-in with GitLab
accounts, see [permissions](permissions.md#gitlab-oauth).

## Connecting GitHub

Sourcegull mirrors repositories from github.com and GitHub Enterprise Server (3.10 or later). Under
**Site admin → Code host connections → Add connection**, pick **GitHub**. The form has:

- **GitHub URL** and **Authentication**: a **personal access token** or a **GitHub App**.
  *How to create the token* links to your instance's token pages: a fine-grained token with
  read-only **Contents** and **Metadata** access to the repositories, or a classic token with
  the `repo` scope (no scope for public repositories only). For a GitHub App, see
  [below](#github-app-authentication).
- **Repositories to mirror**: every repository the token's user owns, collaborates on or
  reaches through an organization (with a GitHub App, every repository of the installation);
  selected organizations and repositories; or repository search queries. **Exclusions** skip repositories by name, by regular expression, archived
  repositories and forks.
- **Advanced**: clone over HTTPS or SSH, request budget (5,000 per hour by default, GitHub's
  limit for a personal token), repository name pattern, TLS certificate.

**Test connection** shows the GitHub user the token belongs to and, for a classic token, its
scopes.

```jsonc
{
  "url": "https://github.com",
  "token": "github_pat_...",
  // Every repository of these organizations that the token can read.
  "orgs": ["acme"],
  // Single repositories, as owner/name.
  "repos": ["other-org/tool"],
  // "affiliated", "none", "public" (GitHub Enterprise Server only) or a repository search
  // such as "org:acme topic:backend" (at most 1,000 results per query).
  "repositoryQuery": ["none"],
  "exclude": [{ "archived": true }, { "forks": true }, { "pattern": "^acme/sandbox-" }]
}
```

Other keys: `repositoryPathPattern` (default `{host}/{nameWithOwner}`), `nameTransformations`,
`gitURLType` (`http` or `ssh`), `certificate`, `rateLimit` and `webhooks`. Every sync lists
the connection's repositories in full; GitHub's rate limit headers (and secondary rate limits)
pause the connection's requests until GitHub allows them again.

For updates within seconds, add a webhook in the repository or organization settings on GitHub:
the payload URL shown on the connection page, content type `application/json`, one of the
connection's `webhooks` secrets as the secret, and the **Pushes** and **Repositories** events
(with repository permissions, also **Collaborator add, remove, or changed**, **Memberships**,
**Organizations** and **Team adds**).
GitHub signs each delivery with the secret; Sourcegull rejects unsigned or wrongly signed ones.

**Permissions:** without an `authorization` block every Sourcegull user can read every repository
of a GitHub connection, including private ones (the connection page and the form say so). With
`"authorization": {"identityProvider": {"type": "oauth"}}` (**Repository permissions** in the
form), users who signed in with GitHub see the private repositories they can read on GitHub;
see [GitHub repository permissions](permissions.md#github-repository-permissions). Pull
requests for bulk changes, SCIP uploads from GitHub Actions and team sync are GitLab-only for
now; for sign-in with GitHub accounts, see [permissions](permissions.md#github-oauth).

## Connecting Gitea

Sourcegull mirrors repositories from Gitea 1.20 or later, self-hosted or gitea.com. Under **Site
admin → Code host connections → Add connection**, pick **Gitea**. The form has:

- **Gitea URL** (with its path when Gitea is served under one, e.g. `https://example.com/git`)
  and **Token**. *How to create the token* links to the instance's **Settings → Applications**:
  generate a token with read access to `repository`, `organization` and `user`.
- **Repositories to mirror**: every repository the token's user owns, collaborates on or
  reaches through an organization; selected organizations and repositories; or repository
  searches. **Exclusions** skip repositories by name, by regular expression, archived
  repositories, forks and mirrors.
- **Advanced**: clone over HTTPS or SSH, request budget (36,000 per hour by default; Gitea does
  not limit its API, but a proxy in front of it may), repository name pattern, TLS certificate.

**Test connection** shows the Gitea user the token belongs to, and warns when the instance is
older than 1.20.

```jsonc
{
  "url": "https://gitea.example.com",
  "token": "...",
  // Every repository of these organizations that the token can read.
  "orgs": ["acme"],
  // Single repositories, as owner/name.
  "repos": ["other-org/tool"],
  // "affiliated", "none", "all" (every repository the token can read; not on gitea.com) or a
  // keyword matched against repository names.
  "repositoryQuery": ["none"],
  "exclude": [{ "archived": true }, { "mirrors": true }, { "pattern": "^acme/sandbox-" }]
}
```

Other keys: `repositoryPathPattern` (default `{host}/{nameWithOwner}`), `nameTransformations`,
`gitURLType` (`http` or `ssh`), `certificate`, `rateLimit`, `authorization` and `webhooks`.
Every sync lists the connection's repositories in full. A repository in a private organization is private in Sourcegull,
and one in a limited organization (visible to signed-in users) is internal.

For updates within seconds, add a webhook of type **Gitea** in the repository or organization
settings: the target URL shown on the connection page, content type `application/json`, one of
the connection's `webhooks` secrets as the secret, and the **Push**, **Create**, **Delete**,
**Fork** and **Repository** events. Gitea signs each delivery with the secret; Sourcegull rejects unsigned or
wrongly signed ones. Gitea refuses to deliver to private addresses unless
`[webhook] ALLOWED_HOST_LIST` in its `app.ini` allows the Sourcegull host.

**Permissions:** without an `authorization` block, every Sourcegull user can read every
repository of a Gitea connection, including private ones. With one, private and internal
repositories are readable only by the users who can read them on Gitea; see
[repository permissions](permissions.md#repository-permissions). Pull requests for bulk changes,
SCIP uploads from CI and team sync are GitLab-only for now; for sign-in with Gitea accounts,
see [permissions](permissions.md#gitea-oauth).
