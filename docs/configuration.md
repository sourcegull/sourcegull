# Configuration

Sourcegull is configured in three places:

- **Environment variables** (or the matching command-line flags) set what a process needs to
  start: its roles, the database, listeners, data directories, keys and repo nodes.
- **Site configuration**: one JSONC document, stored in the database and edited under
  **Site admin → Site configuration**, for instance-wide behaviour (sign-in, permissions,
  search, executors, email, …). Changes apply without a restart, except
  `gitMaxConcurrentClones` (read when a repo node starts) and
  `insights.query.worker.concurrency` (read when a worker starts).
- **Settings**: JSONC documents per user, per organization and global, merged into each user's
  effective settings.

All of the instance's own configuration can instead come from files kept in git
([configuration as code](#configuration-as-code)): the site configuration, the global settings,
and the objects otherwise created in the UI (code host connections, search contexts, roles, site
admins, outbound webhooks, executor secrets, code intelligence policies and feature flags).

## Environment variables

Every flag of `sourcegull` can be set through its `SOURCEGULL_*` variable (`sourcegull <command> --help`
lists them). Booleans accept `1`/`0`, `true`/`false`, `yes`/`no` and `on`/`off`. The file
variables (`SOURCEGULL_*_FILE`) and `SOURCEGULL_KEYS` count as unset when empty.

### Server (`sourcegull run`, `sourcegull supervise`)

| Variable | Default | Meaning |
|---|---|---|
| `SOURCEGULL_ROLES` | `all` | Comma-separated roles: `web`, `repo`, `worker`, `blob`, or `all` ([architecture](architecture.md#one-binary-several-roles)). |
| `SOURCEGULL_DATABASE_URL` | | PostgreSQL URL (required). |
| `SOURCEGULL_HTTP_ADDR` | `0.0.0.0:7080` | Public listener of the `web` role: UI, `/.api/*`, `/healthz`, `/readyz`. |
| `SOURCEGULL_INTERNAL_ADDR` | `0.0.0.0:6060` | Internal listener: `/metrics`, `/healthz`, `/readyz`; with the `blob` role also the blob API that `SOURCEGULL_BLOB_STORE=http://…` points at. |
| `SOURCEGULL_DATA_DIR` | `/var/opt/sourcegull` | Persistent data. |
| `SOURCEGULL_SHUTDOWN_DELAY` | `5s` | On SIGTERM or SIGINT, how long `/readyz` returns 503 while the process keeps serving, before the listeners close (`500ms`, `5s`, `1m`, or seconds; `0` stops at once). The Helm chart sets `0`: its pods pause in a `preStop` hook instead. |
| `SOURCEGULL_KEYS_FILE` | `$SOURCEGULL_DATA_DIR/keys` | Encryption key file ([below](#encryption-keys)). |
| `SOURCEGULL_KEYS` | | The encryption keys themselves, instead of a key file ([below](#keys-in-the-environment)). |
| `SOURCEGULL_EXTERNAL_URL` | | Default for `externalURL` when the site configuration does not set it. |
| `SOURCEGULL_SITE_CONFIG_FILE` | | Read the site configuration from this file instead of the database; it is then read-only in the UI and API ([below](#configuration-as-code)). |
| `SOURCEGULL_RESOURCES_FILE` | | Resources file: connections, search contexts, roles, site admins, webhooks, executor secrets, code intelligence policies and feature flags ([below](#resources-file)). |
| `SOURCEGULL_GLOBAL_SETTINGS_FILE` | | Global settings document; they are then read-only in the UI and API ([below](#global-settings-file)). |
| `SOURCEGULL_BLOB_STORE` | `<data dir>/blobs` | `file:///path`, `http://host:port` of a `blob` role, or `s3://bucket[/prefix]` ([S3](install.md#s3-blob-storage)). |
| `SOURCEGULL_DB_MAX_CONNECTIONS` | `20` | Main database pool per process ([below](#database-connections)). |
| `SOURCEGULL_DB_WORKER_MAX_CONNECTIONS` | `10` | The worker role's own pool when it shares a process with other roles. |
| `SOURCEGULL_LOG_FORMAT` | `json` | `json` or `pretty`. Log filters come from `RUST_LOG` (default `info`). |
| `SOURCEGULL_RATE_LIMIT_BACKEND` | `auto` | Code host request budgets: `postgres` (shared by every process), `memory` (per process), or `auto` (`memory` when one process runs web, repo and worker). |
| `SOURCEGULL_LEADER_BACKEND` | `postgres` | Leases of singleton tasks: Postgres advisory locks or Kubernetes `Lease` objects (`kubernetes`). |
| `SOURCEGULL_LEADER_NAMESPACE`, `SOURCEGULL_LEADER_LEASE_PREFIX`, `SOURCEGULL_LEADER_IDENTITY`, `SOURCEGULL_LEADER_LEASE_DURATION` | pod namespace, `sourcegull`, host name, `15` | Kubernetes lease settings. |
| `SOURCEGULL_SEARCH_THREADS` | available CPUs | Search worker threads on a repo node. |

### Repo nodes

| Variable | Default | Meaning |
|---|---|---|
| `SOURCEGULL_REPO_NODES` | this node | Comma-separated repo node addresses, e.g. `http://sourcegull-repo-0.sourcegull-repo:3178,http://sourcegull-repo-1.sourcegull-repo:3178`. Every process must list them with the same spelling. |
| `SOURCEGULL_REPO_NODE_ADDR` | the only node, or the one whose host is `$HOSTNAME` | This node's own address as listed in `SOURCEGULL_REPO_NODES`. |
| `SOURCEGULL_REPO_GRPC_ADDR` | `0.0.0.0:3178` | gRPC listener of the `repo` role. |
| `SOURCEGULL_REPO_DATA_DIR` | `$SOURCEGULL_DATA_DIR` | Bare repositories, index shards and caches. |
| `SOURCEGULL_REPO_MAX_CONCURRENT_FETCHES` | `10` | Concurrent fetches per node (clones: site configuration `gitMaxConcurrentClones`). |
| `SOURCEGULL_REPO_SSH_KEY`, `SOURCEGULL_REPO_SSH_KNOWN_HOSTS` | | Private key and `known_hosts` for connections that clone over SSH. |
| `SOURCEGULL_REPO_DISABLE_INDEX` | `false` | Keep no search index on this node; content search then reads from git. |
| `SOURCEGULL_REPO_INDEX_DEBOUNCE_MS` | `5000` | Pushes within this window are indexed in one build. |
| `SOURCEGULL_REPO_PAUSE_CLONES_FREE_PERCENT`, `SOURCEGULL_REPO_PAUSE_CLONES_FREE_GIB` | `10`, `20` | Clones pause when free disk space is below both thresholds. `0` turns the pause off. |
| `SOURCEGULL_REPO_EVICT_FREE_PERCENT`, `SOURCEGULL_REPO_EVICT_FREE_GIB` | `7`, `10` | Caches and shards of inactive repositories are evicted when free space is below both. `0` turns eviction off. |
| `SOURCEGULL_REPO_REBALANCE_CONCURRENCY` | `2` | Repositories moved to this node at once during rebalancing; `0` turns moves to it off. |

### Blob storage (S3)

`SOURCEGULL_S3_ENDPOINT`, `SOURCEGULL_S3_REGION`, `SOURCEGULL_S3_PATH_STYLE`, `SOURCEGULL_S3_ACCESS_KEY_ID`,
`SOURCEGULL_S3_SECRET_ACCESS_KEY`, `SOURCEGULL_S3_SESSION_TOKEN`, `SOURCEGULL_S3_CA_CERT`,
`SOURCEGULL_S3_INSECURE_SKIP_VERIFY`, `SOURCEGULL_S3_PART_SIZE`, `SOURCEGULL_S3_CREATE_BUCKET`: see
[Install → S3 blob storage](install.md#s3-blob-storage).

### Executors (`sourcegull executor`)

| Variable | Default | Meaning |
|---|---|---|
| `SOURCEGULL_EXECUTOR_FRONTEND_URL` | | URL of the web role, e.g. `http://sourcegull-web:7080`. |
| `SOURCEGULL_EXECUTOR_TOKEN` | | The shared token, equal to `executors.accessToken` in the site configuration. |
| `SOURCEGULL_EXECUTOR_NAME` | host name | Unique executor name. |
| `SOURCEGULL_EXECUTOR_QUEUES` | `codeintel` | `codeintel` (auto-indexing) and/or `batches` (bulk change spec executions). |
| `SOURCEGULL_EXECUTOR_MAX_JOBS` | `1` | Jobs run at the same time. |
| `SOURCEGULL_EXECUTOR_RUNTIME` | `docker` | `docker` (a container per step) or `kubernetes` (a Job per auto-indexing job, a Job per step of a bulk change). |
| `SOURCEGULL_EXECUTOR_DOCKER_NETWORK` | `bridge` | Network of step containers: `bridge`, `none` or a named network. |
| `SOURCEGULL_EXECUTOR_DOCKER_MEMORY`, `SOURCEGULL_EXECUTOR_DOCKER_CPUS` | none | Limits per step container (`4g`, `2`). |
| `SOURCEGULL_EXECUTOR_DOCKER_ALWAYS_PULL` | `false` | Pull step images even when present. |
| `SOURCEGULL_EXECUTOR_POLL_INTERVAL`, `SOURCEGULL_EXECUTOR_SCRATCH_DIR`, `SOURCEGULL_EXECUTOR_GIT` | `1`, `/tmp/sourcegull-executor`, `git` | Poll interval (seconds), checkout directory, git binary. |
| `SOURCEGULL_EXECUTOR_KUBERNETES_*` | | Kubernetes runtime: `NAMESPACE`, `HELPER_IMAGE`, `POD_FRONTEND_URL`, `RUNTIME_CLASS`, `NODE_SELECTOR`, `TOLERATIONS`, `SERVICE_ACCOUNT`, `IMAGE_PULL_SECRETS`, `CPU_REQUEST`, `MEMORY_REQUEST`, `CPU_LIMIT`, `MEMORY_LIMIT`, `WORKSPACE_SIZE`, `DEADLINE` (default 4 h), `START_TIMEOUT` (default 600 s), `KEEP_JOBS`. |

### Command-line client (`gull`)

`SOURCEGULL_ENDPOINT` (the instance URL) and `SOURCEGULL_ACCESS_TOKEN` (an
[access token](permissions.md#access-tokens)).

## Site configuration

The site configuration is JSONC (comments and trailing commas allowed). The editor under
**Site admin → Site configuration** completes keys, shows their documentation, validates as you
type, and can reorganize keys by section. The JSON Schema is served at
`/.api/site-config-schema`. A new instance starts from a commented template that shows the
common keys.

Credentials (`email.smtp.password`, `auth.providers[].clientSecret`, `executors.accessToken`,
`scim.authToken`) are shown as `"REDACTED"` in the editor and the API. Saving a document that
still holds `"REDACTED"` keeps the stored value (an auth provider keeps the secret of the stored
provider with the same `type` and `clientID`); a new value replaces it. `${env:…}` and
`${file:…}` references are shown as they are.

Frequently used keys:

| Key | Purpose |
|---|---|
| `externalURL` | The URL users reach Sourcegull at; used for OAuth callbacks, webhooks and links in emails. |
| `auth.providers`, `auth.public`, `auth.sessionExpiry`, `auth.passwordPolicy`, `auth.lockout`, `auth.rateLimits`, `auth.accessTokens`, `auth.accessRequest`, `auth.allowedIpAddress`, `auth.enableUsernameChanges` | Sign-in and accounts ([permissions](permissions.md)). |
| `mcp.oauth.enabled`, `mcp.oauth.dynamicClientRegistration`, `mcp.oauth.accessTokenTTL`, `mcp.oauth.refreshTokenTTL`, `mcp.oauth.allowedRedirectHosts` | OAuth sign-in of MCP clients ([MCP](mcp.md#oauth-sign-in)). |
| `permissions.userMapping`, `authz.enforceForSiteAdmins`, `permissions.sync*` | Repository permissions. |
| `scim.authToken` | Enables SCIM provisioning. |
| `repoListUpdateInterval`, `gitUpdateInterval`, `disableAutoGitUpdates`, `gitMaxConcurrentClones` | Discovery and fetching. |
| `repoNodes.pinnedRepos` | Pin repositories to repo nodes (repository name → repo node address). Replaces `experimentalFeatures.gitServerPinnedRepos`, which is still read, with a deprecation warning, when `repoNodes.pinnedRepos` is not set. |
| `search.limits` | Search limits, read at each search: `defaultTimeoutSeconds` (20), `maxTimeoutSeconds` (60; `timeout:` is clamped to it), `streamingMaxResults` (500, the result count without `count:`), `maxRepos` (unset: no limit; repositories beyond it are skipped with a warning), `commitDiffMaxRepos` (50) and `commitDiffWithTimeFilterMaxRepos` (10000) for `type:commit`/`type:diff` without and with `before:`/`after:`, and for search jobs `exhaustiveMaxResults` (1000000) and `exhaustiveTimeoutSeconds` (3600, per repository revision). |
| `search.index.branches` | Branches to index besides the default branch, per repository (up to 63). |
| `search.index.alwaysIndex` | Path patterns whose files are searched by content above 1 MiB (up to 16 MiB), per repository; `"*"` applies to all ([search](search.md)). |
| `executors.accessToken` | Shared secret of the executors (at least 20 characters). |
| `codeIntelAutoIndexing.*`, `codeIntel.gitlabJobTokenUploads`, `codeIntel.maxUploadBytes`, `codeIntelRanking.documentReferenceCountsEnabled` | [Code navigation](code-navigation.md). |
| `batchChanges.*` | [Bulk changes](bulk-changes.md). |
| `insights.*`, `searchJobs.retention`, `own.syncTeamsFromGitLabGroups` | [Other features](features.md). |
| `email.address`, `email.senderName`, `email.smtp` | Outbound email. |
| `webhooks.outbound.allowedHosts`, `webhooks.outbound.logRetention` | Outbound webhooks. |
| `log.securityEventLog`, `eventLogs.retention`, `eventLogs.rollupRetention` | Audit and usage logs. |
| `observability.logSlowSearches`, `observability.logSlowGraphQLRequests`, `outboundRequestLogLimit` | Diagnostic logs. A slow GraphQL request is stored with its variables only when it is a query, and with the values of secret-looking variables (tokens, passwords, credentials) replaced. |

With `SOURCEGULL_SITE_CONFIG_FILE` the document comes from a file (for example a Kubernetes
Secret) and cannot be edited in the UI; see [configuration as code](#configuration-as-code).

## Settings and the settings cascade

Settings are per-viewer preferences. They exist at three levels:

1. **Global settings** (Site admin → Global settings),
2. each **organization's** settings, for its members,
3. each **user's** own settings.

A user's effective settings merge them in that order: objects merge key by key, and any other
value at a later level replaces the earlier one. Known keys include
`search.defaultPatternType` (default `keyword`), `search.includeForks`,
`search.includeArchived`, `quicklinks` and `notices`; other keys are passed through to the UI.

The **settings reference** page at `/help/settings` lists every setting with its type,
allowed values and description; the editor's starting document links to it. The schema is
served at `/.api/settings-schema`.

## Encryption keys

Secrets in the database (code host tokens, OAuth credentials, executor secrets, webhook secrets
and URLs, watch webhook URLs) are encrypted with data keys, which are in turn wrapped
by key-encryption keys from the key file. The key file has one `id:base64key` line per key
(32 random bytes); the **last** line is the current key:

```sh
printf 'k1:%s\n' "$(openssl rand -base64 32)" > keys
```

- `sourcegull supervise` creates the file if it is missing. `sourcegull run` without a key
  file starts, but features that store secrets (code host connections) are unavailable.
- Back the file up with the database. Losing it makes the stored secrets unreadable. Every
  Sourcegull process needs the same keys.

**Rotation.** Append a new key as the last line, then restart a process or run
`sourcegull keys rotate`. Changing the file alone does nothing: a rotation happens only when a
process starts or that command runs. The first process with the new key re-wraps every data
key, retired ones included, with it, creates new data keys and retires the old ones
(decrypt-only). It then notifies the other processes, which re-read their key file and reload the
data keys without a restart. Workers then re-encrypt every secret in the background as an
out-of-band migration; progress shows on **Site admin → Migrations** and in
`sourcegull migrate status`. When it reaches 100%, remove the old key line; nothing removes it for
you. `sourcegull keys rotate` with an unchanged file creates new data keys under the same key.

- Because the data keys are re-wrapped, the file never needs more than two lines, the old key and
  the new one, and only during a rotation.
- Every process must have the new key before or when the rotation runs. A process with only the
  old key cannot unwrap the re-wrapped data keys: its reload fails with a warning
  ("reloading the data encryption keys failed") and it keeps the keys it had until it is restarted
  with the new key.
- Retired data keys stay in the `encryption_keys` table (one row per record class per rotation);
  once re-encryption reaches 100% no secret uses them.
- `sourcegull keys status` lists the data keys and how many secrets still use retired ones.

The Compose walkthrough is in [upgrade.md](../deploy/docker/upgrade.md#rotating-the-encryption-key).

### Keys in the environment

`SOURCEGULL_KEYS` holds the same entries as the key file, separated by commas (or newlines), the
last one current:

```sh
# One key:
export SOURCEGULL_KEYS="k1:$(openssl rand -base64 32)"
# After a rotation (keep k1 until re-encryption reaches 100%):
export SOURCEGULL_KEYS="k1:<old base64>,k2:$(openssl rand -base64 32)"
```

- Set either `SOURCEGULL_KEYS` or `SOURCEGULL_KEYS_FILE`, not both (that is a startup error).
  `sourcegull supervise` does not create a key file when `SOURCEGULL_KEYS` is set.
- Each process reads the variable once at startup and removes it from its environment before it
  starts any thread, so the git processes and other children it runs never inherit it. The
  process's initial environment (`/proc/<pid>/environ`) still holds it, readable by the same
  user and root, as for any variable. Secret
  injectors that resolve the value before the process starts (a mutating webhook that wraps the
  container's command, an init process that execs Sourcegull with the resolved environment) work
  unchanged. A value still starting with `vault:` is reported as an unresolved reference rather
  than as a malformed key.
- The variable is fixed for the life of a process, so a rotation takes two restarts of every
  process: one with `old,new` (the first to start rotates), and one with only `new` after
  re-encryption reaches 100%. A key file is re-read on rotation without a restart.
  `sourcegull keys status` and `sourcegull keys rotate` accept either source.
- Anyone who can read a process's or container's environment (`docker inspect`, a pod spec with
  a literal value) can read the keys. In Kubernetes, reference a Secret (`valueFrom`) or use an
  injector, never a literal value; the Helm chart's `encryptionKeys.source=env` does the former.

## Database connections

Each process has a main pool (`SOURCEGULL_DB_MAX_CONNECTIONS`, default 20) for the `web`, `repo`
and `blob` roles. When the `worker` role runs in the same process (`--roles=all`,
`supervise`), it gets its own pool (`SOURCEGULL_DB_WORKER_MAX_CONNECTIONS`, default 10) so
background jobs cannot starve requests; a process that runs only `worker` uses the main pool.
Outside these pools, each process holds one `LISTEN` connection and one connection per leader
lease it holds (with the Postgres leader backend).

Keep the sum over every process below PostgreSQL's `max_connections`. Connections show up in
`pg_stat_activity` with `application_name` `sourcegull:<roles>`.

## Database migrations

`sourcegull migrate` manages the schema: `up`, `down --to <n>`, `status`, `validate`, `versions`,
`drift [--version <v>]` (compares the live schema with a version's and prints SQL that fixes
each difference) and `upgrade --from <version> [--to <version>] [--dry-run]` (upgrades across
several versions after a drift check; stop Sourcegull first). Out-of-band data migrations run in the
background afterwards; **Site admin → Updates** lists the ones that must finish before the next
upgrade.

## Configuration as code

Three files can replace what is otherwise edited in the UI, so a deployment is described by
files kept in git and rolled out with Compose or Helm:

| File | Variable | Holds | Applied by |
|---|---|---|---|
| Site configuration | `SOURCEGULL_SITE_CONFIG_FILE` | The [site configuration](#site-configuration) document | every process, re-read every 30 s |
| Global settings | `SOURCEGULL_GLOBAL_SETTINGS_FILE` | The [global settings](#settings-and-the-settings-cascade) document | the leader worker, every 30 s |
| Resources | `SOURCEGULL_RESOURCES_FILE` | Code host connections, search contexts, roles, site admins, outbound webhooks, executor secrets, code intelligence policies, feature flags | the leader worker, every 30 s |

What a file sets is read-only in the UI and API (mutations fail with `READ_ONLY`, and pages
show who manages the object). What the files do not cover stays in the database: users and
their settings, organizations, access tokens and OAuth grants, saved searches, notebooks, code
monitors, insights, bulk changes and code intelligence data, so the database still needs
[backups](#backups).

`sourcegull config check` validates the files offline, for CI pipelines: the format, every entry
with the checks the UI applies, and secret references (resolved from the environment the
command runs in; missing ones are named, values are never printed). It exits with 1 on a
problem.

```sh
sourcegull config check --site-config site-config.jsonc --resources resources.jsonc \
  --global-settings global-settings.jsonc
```

### Secret references

Any string in the site configuration and resources files can reference secrets, so the files
can be committed:

- `${env:NAME}`: the environment variable `NAME` (unset is an error, never an empty string);
- `${file:/path}`: the file's contents without trailing newlines, e.g. a mounted Kubernetes or
  Docker secret;
- `$${` is a literal `${`; any other `${…}` is left as it is.

References are resolved every time the file is read, so a changed secret file (or, after a
restart, a changed variable) is applied like a changed config file. A reference that cannot be
resolved keeps the previous state and names the field and the variable or file, never a value.
The global settings file does not resolve references: every user can read the global settings.

### Resources file

A JSONC object with optional sections; its schema is served at `/.api/resources-schema` (point
`"$schema"` at it for editor completion). A section that is present owns the objects it lists;
an absent section manages nothing.

```jsonc
{
  // Verified email addresses of site admins.
  "siteAdmins": ["admin@example.com"],
  "connections": [{
    "key": "gitlab-main",            // stable id: renaming it recreates the connection
    "kind": "gitlab",                // gitlab | github | gitea
    "displayName": "GitLab",
    "config": { "url": "https://gitlab.example.com", "token": "${env:GITLAB_TOKEN}",
                "projectQuery": ["groups/acme/projects"] }
  }],
  "searchContexts": [
    { "name": "backend", "description": "Backend services", "query": "repo:^gitlab\\.example\\.com/acme/" },
    { "name": "api", "repositories": [{ "repository": "gitlab.example.com/acme/api", "revisions": ["main"] }] }
  ],
  "roles": [{ "name": "bulk-editors", "permissions": ["BATCH_CHANGES#WRITE"], "users": ["bob@example.com"] }],
  "outboundWebhooks": [{ "key": "audit", "url": "https://hooks.example.com/sourcegull",
                         "secret": "${env:HOOK_SECRET}", "eventTypes": ["batch_change:apply"] }],
  "executorSecrets": [{ "scope": "batches", "name": "GITLAB_TOKEN", "value": "${env:GITLAB_TOKEN}" }],
  "codeIntelPolicies": [{ "key": "default-branch", "name": "Default branches", "type": "GIT_TREE",
                          "pattern": "HEAD", "indexingEnabled": true }],
  "featureFlags": [{ "name": "my-flag", "value": true }, { "name": "beta", "rolloutBasisPoints": 2500 }]
}
```

| Section | Entries | Identity (takeover) |
|---|---|---|
| `connections` | `key`, `kind`, `displayName`, `config` (the connection configuration, as in the UI's editor) | same kind and code host URL |
| `searchContexts` | `name`, `description`, `public` (default `true`), `query` or `repositories` (`repository`, `revisions`; the default branch when empty) | same name, instance-level |
| `roles` | `name`, `permissions` (`NAMESPACE#ACTION`), `users` (verified emails) | same name |
| `siteAdmins` | verified email addresses | — |
| `outboundWebhooks` | `key`, `url`, `secret`, `eventTypes` (names, or `{ "eventType", "scope" }`) | same URL |
| `executorSecrets` | `scope` (`batches` or `codeintel`), `name` (the variable jobs see), `value` | same scope and name, global |
| `codeIntelPolicies` | `key`, `name`, `type` (`GIT_TREE`, `GIT_TAG`, `GIT_COMMIT`), `pattern`, `repositoryPatterns`, `retentionEnabled`, `retentionDurationHours`, `retainIntermediateCommits`, `indexingEnabled`, `indexCommitMaxAgeHours`, `indexIntermediateCommits`, `syntacticIndexingEnabled` | same name, not tied to one repository |
| `featureFlags` | `name`, `value` or `rolloutBasisPoints` | same name |

Every entry has a key: `key` where given, else its name (`<scope>/<name>` for executor
secrets). On each pass, per section:

- an unchanged entry (same resolved contents) does nothing; a changed one, including a rotated
  secret it references, updates its object in place;
- a new key first looks for an object with its identity that no entry owns (see the table) and
  takes it over, so moving a running instance to a file recreates nothing; otherwise the object
  is created;
- a key removed from its section deletes its object (a connection's repositories go with it
  unless another connection lists them);
- a removed section, or an unset `SOURCEGULL_RESOURCES_FILE`, releases its objects to the UI
  unchanged;
- changes the system makes to a declared object, such as a refreshed OAuth token in a
  connection's configuration, are kept until the entry itself changes.

With `"exclusive": true`, each present section also owns the objects of its kind that the file
does not list: they become read-only and nothing of that kind can be created in the UI.

**Site admins and role members** are matched against verified email addresses. Every source
that marks an address verified counts: an identity provider that vouches for it at sign-in,
SCIM provisioning, the email header of `http-header` sign-in, and a user confirming it by email.
Only declare addresses whose verification you trust there; with password sign-in alone, admins
need working email to verify their address.

A listed user becomes site admin, or gets the role, at sign-in or within 30 s, and gets it back
if it is taken away in the UI; removing the address takes back only what the file granted.
While `siteAdmins` is set, the first user to sign up is not made site admin, and a declared
admin can create their account even when sign-up is closed, so a fresh SSO-only instance needs
no manual step. Declared admins cannot be demoted in the UI, and site admins cannot verify or
unverify a declared address for its user; with `exclusive`, no one can be promoted or demoted
there, and admins the file does not list are demoted once one declared admin is site admin.

**Errors.** A file that cannot be read, parsed or resolved, or fails a check, changes nothing:
the last applied state stays. An entry the database rejects (for example a name another object
uses) fails alone while the others apply. Either way the error shows in the site admin status
indicator and on **Site admin → Site configuration**, and `sourcegull_declared_config_errors{source}`
is 1. Every change is a `DeclaredResourceChanged` or `DeclaredGrantChanged` security event, and
`sourcegull_declared_config_changes_total{kind,action}` counts them.

### Global settings file

`SOURCEGULL_GLOBAL_SETTINGS_FILE` holds the global settings document (JSONC, as in **Site admin →
Global settings**). When its contents differ from the current global settings, the leader
worker saves them as a new version; the page is read-only meanwhile. Unsetting the variable
makes the global settings editable again, starting from the last version.

### Deploying the files

- **Helm**: `config.siteConfig`, `config.globalSettings` and `config.resources` in values are
  rendered into a Secret mounted on every Sourcegull pod (or use `config.existingSecret`); the
  variables the files reference go in `extraEnvVars`. See the [chart README](../deploy/helm/sourcegull/README.md#configuration-as-code).
- **Docker Compose**: put the files in `deploy/docker/config/` (mounted at
  `/etc/sourcegull/config`), name them in `.env`, and put referenced variables in
  `config/secrets.env`.

### Backups

With configuration as code, the database holds only what users create; restoring an instance
from git and an empty database loses users' tokens, settings and content, not the instance's
configuration. Back up the database and the encryption keys together anyway (a database backup
needs the keys that were current when it was taken). The blob store holds SCIP uploads, which
CI or auto-indexing can upload again.
