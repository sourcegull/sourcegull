# Architecture

## One binary, several roles

Everything server-side is one executable, `sourcegull`. A process runs one or more **roles**:

```sh
sourcegull run --roles=web,worker      # or SOURCEGULL_ROLES=web,worker; `all` runs every role
```

| Role | Does | State |
|---|---|---|
| `web` | The UI, GraphQL (`/.api/graphql`), streaming search, MCP and other HTTP APIs, sign-in, the executor queue API; fans searches out to the repo nodes. | stateless; scale horizontally |
| `repo` | A repo node: bare git repositories, index shards, node-side search, symbols, diffs, blame, archives, commits for bulk changes. Repositories are partitioned across nodes. | data volume per node |
| `worker` | Background jobs (pgqueue workers) and leader-elected schedulers: discovery, fetch scheduling, permission sync, SCIP processing, auto-indexing, bulk change reconciliation, trends, watches, search jobs, analytics rollups, out-of-band migrations. | stateless; scale horizontally |
| `blob` | Blob storage over HTTP for SCIP uploads, code graph files and search job results. Not needed with an [S3 bucket](install.md#s3-blob-storage). | data volume |

Other subcommands of the same binary:

- `sourcegull executor`: takes jobs from the web role and runs them in containers (Docker) or as
  Kubernetes Jobs. See [Executors](#executors).
- `sourcegull migrate`: applies, reverts, checks and upgrades the database schema
  ([configuration](configuration.md#database-migrations)).
- `sourcegull supervise`: creates a key file if missing, applies migrations and runs every role in
  one process against the database at `SOURCEGULL_DATABASE_URL`.
- `sourcegull keys status|rotate`: data encryption keys.
- `sourcegull users reset-password <username>`: prints a password reset link
  ([permissions](permissions.md#locked-out-admins)).
- `sourcegull monitoring generate`: writes the Grafana dashboards, Prometheus rules and
  [alert docs](observability/) from their definitions in `crates/monitoring`.

The `gull` binary is the command-line client (SCIP uploads, bulk changes); it ships in the
same image.

Every process listens on `:6060` (internal: `/metrics`, `/healthz`, `/readyz`, and the blob API
in a process with the `blob` role). The `web` role also listens on `:7080` (public: UI and
APIs), and repo nodes listen for gRPC on `:3178`.

## PostgreSQL is the only dependency

PostgreSQL 18 holds all durable state and also does the jobs often given to other services:

- **Job queues** (`pgqueue`): one table per queue, dequeued with `FOR UPDATE SKIP LOCKED`, with
  heartbeats, retries with backoff and a resetter for jobs whose worker died.
- **Notifications** (`LISTEN/NOTIFY`) wake up workers and propagate site configuration,
  permission changes and the repo routing table. Every subscriber polls as a fallback, since
  notifications are not durable.
- **Leader election**: advisory locks, or Kubernetes `Lease` objects
  (`SOURCEGULL_LEADER_BACKEND=kubernetes`).
- **Sessions, caches and rate limits**: code host request budgets are shared by every process
  through Postgres, and a code host's 429 pauses all of them.

Caches, sessions, rate limits and pub/sub are behind traits with in-process and Postgres
implementations, so another backend can be added without touching their callers.

Secrets are encrypted at rest with envelope encryption: data keys per record class, wrapped by
key-encryption keys from a file ([encryption keys](configuration.md#encryption-keys)).

## gRPC between roles

Roles talk to repo nodes over gRPC, defined in [`proto/repo.proto`](../proto/repo.proto):

- `RepoService`: clone, fetch, status, refs, files and trees, commits, logs, diffs, blame,
  archives, merge bases, `CopyRepo` (for rebalancing), and the write operations bulk changes
  need (`CreateCommitFromPatch`, `ApplyPatch`, `Push`).
- `SearchService`: unindexed search of a commit, commit and diff search, indexed search, index
  status and reindexing, trend series counts and language statistics.

The protos are compiled at build time (no `protoc` needed). Executors talk to the web role over
HTTP (`/.executors/*`), authenticated with the shared executor token.

## Repo nodes and rebalancing

Each repository belongs to one repo node, chosen by **rendezvous hashing** of its name over the
node list in `SOURCEGULL_REPO_NODES`. Every process computes the same assignment, so `web` and
`worker` route requests without a lookup service. The assignment is stored per repository and
published with `NOTIFY` when it changes.

When nodes are added or removed, the repositories whose owner changes are **moved without
downtime**:

1. Every node sends heartbeats with its node list. Once every configured node is alive with the
   same list, the first live node plans a move for each misplaced repository.
2. The new owner copies the repository from the old one with `CopyRepo` in rounds (a ref
   snapshot plus a pack of what it lacks, checksummed, with connectivity and refs verified)
   until a round brings nothing new, then builds its index.
3. The flip: the repository's node changes in one transaction, routing tables reload that
   repository (the notification names it), and a catch-up fetch is queued.
4. The old owner keeps serving during a grace period, then deletes its copy.

Writes check the assignment: a node clones, fetches, commits or pushes (gRPC) only repositories
assigned to it (or to none yet), and refuses the others with `FAILED_PRECONDITION`
(`not_assigned`). A fetch or index build that ends after the flip records nothing, so the new
owner's state is never overwritten; a first clone that ends after it is removed.

Repositories of a node that is gone are cloned again from the code host. Failed moves are retried with
backoff; **Site admin → Repo nodes** shows disk usage per node and the moves, and can retry
failed ones. `repoNodes.pinnedRepos` in the site configuration pins a
repository (for example a very large monorepo) to a node.

Repo nodes protect their disk: clones pause and caches of inactive repositories are evicted
below configurable free-space thresholds ([configuration](configuration.md#repo-nodes)).

## The search index

Sourcegull has its own index format (`crates/index-format`), built by `crates/indexer` on the repo
nodes and queried by `crates/search-exec`. The rest of the system reaches it only through the
`SearchIndex` trait (`crates/search/src/index.rs`).

**Shards.** A shard file holds one repository's indexed branches: by default the default branch,
plus up to 63 more listed in `search.index.branches`. A file version shared by several branches
is stored once, with a bitmask of the branches that contain it. A shard has these sections,
each with its own CRC-32:

- the repository and its branches with their commits,
- a doc table (path, language, size, branch mask, flags) sorted by path, and front-coded paths,
- the content (LZ4-compressed) and newline offsets,
- a **trigram index**: an FST mapping each case-folded trigram to a posting list of doc ids
  (delta varints, or a Roaring bitmap when smaller),
- the symbols defined in each file (extracted with tree-sitter while indexing),
- optional static file ranks from the code graph.

Shards are read through mmap and every checksum and reference is checked when a shard opens, so
a corrupt file is an error, never a crash. Binary files and files over 1 MiB are kept as docs
(path search finds them) without content; files matched by `search.index.alwaysIndex` are read
up to 16 MiB. A change to that rule rebuilds the affected shards at the next reconcile pass.

**Delta shards.** After a push, a node does not rebuild the whole repository. A delta build
compares the branch trees with the base shard path by path: each changed path is written to a
delta shard with all its current versions, and its docs in the base are tombstoned. A
repository has a base shard and at most one delta; the next full build folds them together.
Pushes that arrive within the debounce window (`SOURCEGULL_REPO_INDEX_DEBOUNCE_MS`) are coalesced
into one build, and the last push is always indexed.

**Compound shards.** Small repositories are packed together into compound shards, merged from
their existing shards without git access. Compound shards that become small or mostly obsolete
are repacked. New files are swapped in before old ones are deleted, so a crash at any step
leaves a consistent index.

**Query evaluation.** For each shard, the pattern's trigram query selects candidate docs (rarest
posting lists first); branch, `file:` and `lang:` filters run on the doc table; then candidates
are verified with the same matcher that unindexed search uses, in parallel batches. Symbol
search uses the same candidates and the symbols section, without reading content. Results are
scored ([ranking](search.md#ranking)) with the static file rank as a tie-breaker.

**Unindexed revisions.** A revision the index does not hold (another branch, a tag, a commit,
`rev:at.time()`) is searched directly from git: the repo node walks the commit's tree, applies
path filters before reading blobs, skips binary and oversized files, and searches blobs in
parallel. Both paths return the same matches. `index:no` forces it, `index:only` excludes it,
and `SOURCEGULL_REPO_DISABLE_INDEX` turns the index off on a node.

Commit and diff search (`type:commit`, `type:diff`) read a per-repository commit log maintained
on the repo node.

## Executors

Executors run untrusted work (indexers, bulk change steps) outside the server processes. They
poll the web role's queues (`codeintel`, `batches`), send heartbeats, and report each job as
completed, failed or errored (errored jobs are retried). The Docker runtime runs each step as a
container with optional memory, CPU and network limits; the Kubernetes runtime runs each job as
a Kubernetes Job with configurable namespace, runtime class, node selector, tolerations and
resources. Executor secrets (site- or namespace-level) are injected as environment variables.

## Crate map

| Area | Crates |
|---|---|
| Binary and client | `sourcegull` (server: roles, `run`, `migrate`, `supervise`, `executor`, `keys`; the feature list in `src/features.rs`), `repo-node` (the `repo` role: cloning, indexing, janitor, rebalancing), `cli` (`gull`) |
| Foundations | `types`, `config` (site configuration, settings, the resources file format), `declarative` (applies the configuration-as-code files), `db` (schema, migrations, core stores), `feature` (the `Feature` trait: a feature's routes, workers and singletons), `pgqueue`, `pubsub`, `leader`, `ratelimit`, `crypto`, `oobmigration`, `observability`, `monitoring` (dashboards and alerts over the metric names in `types::metric_names`), `testutil` |
| Identity and APIs | `authn` (accounts, sessions, tokens, RBAC), `authz` (repository permissions), `services` (shared state without HTTP: the git reader, email, relay IDs, search backends, compute, server-side bulk change execution), `graphql` (schema assembly and the core types: users, organizations, repositories, git, search, site, settings, permissions), `graphql-core` (context, pagination, node IDs, the `Node` interface and node loader registry, references to shared types, the `GraphqlFeature` trait), one `<feature>-graphql` crate per feature (`insights`, `monitors`, `own`, `codeintel`, `batches`, `notebooks`, `searchjobs`, `saved-searches`, `outbound-webhooks`, `analytics`, `executors`), `http-api` (HTTP server: routing, middleware, streaming endpoints, SCIM, webhooks), `http-extract` (caller extractors and error responses for crates that add routes), `mcp`, `oauth-server` (OAuth sign-in of MCP clients), `web-assets` (embedded UI) |
| Code hosts and git | `codehost` (traits and the shared REST client: retries, rate limits, pagination, the same-origin credential guard), `codehost-gitlab`, `codehost-github`, `codehost-gitea`, `repos` (discovery, webhooks), `gitstore` (bare repositories on repo nodes), `rpc` (gRPC and routing), `languages`, `highlight` |
| Search | `query` (parser), `query-wasm` (parser for the UI), `search` (planning, the pattern matcher, ranking, streaming types), `search-web` (web-side search execution, with code ownership passed in), `search-client` (fan-out to the repo nodes, repository predicates, wire forms), `search-exec` (node-side engines), `index-format`, `indexer`, `structural`, `symbols`, `searchjobs` |
| Code navigation | `codenav`, `codeintel-scip`, `codeintel-syntactic`, `codeintel-policies`, `autoindex`, `ranking` |
| Executors and blobs | `executor`, `executor-api`, `blobstore` |
| Bulk changes | `batches` (specs), `batches-template`, `batches-resolver`, `batches-steps`, `batches-reconciler` |
| Other features | `insights`, `monitors`, `notebooks`, `own`, `compute`, `analytics`, `outbound-webhooks` |

Permission rules live in the feature crates, in an `access` module that takes the actor and
returns `authn::AccessError` (`insights`, `monitors`, `notebooks`, `own` for teams, `searchjobs`;
bulk changes, executor secrets, settings, saved searches and search contexts in `services`).
GraphQL, MCP and the HTTP routes call the same functions and only map the error. The namespace
rule (who may act for a user or an organization) and role permissions are in `authn::access`.
Repository rows are read through the `db` stores with the viewer's `RepoAccess`;
`scripts/check-repo-sql.py` rejects raw `repo` reads elsewhere.

`db` holds the pool, the migrations (one directory) and the core stores: repositories, users,
organizations, code host connections, settings, sessions and tokens, the OAuth server's tokens
(checked by `authn`) and SCIM provisioning (it writes the user tables). Feature stores live in
their crates (`batches` behind its `store` feature, `outbound-webhooks`, `codeintel-scip`,
`codeintel-policies`, `autoindex`, `ranking`, `insights`, `own`, `monitors`, `searchjobs`,
`notebooks`, `analytics`).

A feature implements `feature::Feature` in its crate (routes, workers, singletons) and
`graphql_core::GraphqlFeature` in its `<feature>-graphql` crate (query and mutation objects, node
types and loaders), and is added to `crates/sourcegull/src/features.rs` (`all()` and the `Graphql`
tuple); see "GraphQL by domain" in `development.md`. A singleton runs under a lease of its name
(`leader`); each time a process takes the lease it advances the lease's epoch in
`leader_epochs` and hands the task a `Fence`, and the writes a leader that lost its lease must not
make (out-of-band migration batches and progress, the ranker's ranks, analytics rollups, the
watch evaluator's trigger jobs and cursor, insights backfill state, the changeset syncer and
rollout scheduler, team sync) commit only while that epoch is current. Watch actions other than email are queued on
the outbound webhook queue and delivered by its sender (request guard, retries, delivery log).

Outside `crates/`: `proto/` (gRPC), `ui/` (SvelteKit web app, browser checks in `ui/e2e/`),
`deploy/` (images, Compose, Helm chart, GitLab CI template) and `scripts/` (development,
tests, benchmarks).
