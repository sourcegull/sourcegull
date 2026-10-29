# Development

Sourcegull is a Rust workspace (`crates/`) with a SvelteKit web app (`ui/`). The code layout is
described in [architecture](architecture.md#crate-map).

## In Docker (nothing to install but Docker)

```sh
docker compose up -d          # postgres, sourcegull (built from this checkout), UI dev server
scripts/dev logs sourcegull       # the first start compiles the workspace (a few minutes)
open http://localhost:5173    # UI with hot reload; API and embedded UI on :7080, metrics on :6060
```

`compose.yaml` at the repository root is the development setup (production Compose is
[`deploy/docker/compose.yaml`](../deploy/docker/compose.yaml)). The `sourcegull` service runs
`cargo build -p sourcegull` and then `sourcegull supervise` (encryption keys, migrations, all roles)
against the `postgres` service. The `ui` service builds the query parser WASM and runs
`vite dev`, which proxies `/.api` to `sourcegull`. After changing Rust code,
`scripts/dev restart` rebuilds and restarts the backend; UI changes reload on their own.

The first user to sign up becomes the site admin.

The `postgres` service runs PostgreSQL 18 with its volume at `/var/lib/postgresql`.

Auto-indexing needs an executor: `docker compose --profile executors up -d executor` runs one
from the binary the `sourcegull` service builds, with jobs as containers on the host's Docker. Set
`SOURCEGULL_EXECUTOR_TOKEN` (the site config's `executors.accessToken`), `DOCKER_GID`
(`stat -c %g /var/run/docker.sock`) and `SOURCEGULL_UID` (`id -u`) in `.env`, and restart it after
`scripts/dev restart`, since it runs the rebuilt binary only when it starts.

`scripts/dev` wraps the common commands (`scripts/dev help` lists them all):

| Command | Does |
|---|---|
| `scripts/dev up` / `down` / `restart` / `logs [service]` / `ps` | manage the development services |
| `scripts/dev shell` | bash in the toolbox container (cargo, npm, psql, git); `scripts/dev <cmd>` runs one command there, e.g. `scripts/dev cargo build -p sourcegull` |
| `scripts/dev test [args]` | `cargo test --workspace` against the dev Postgres |
| `scripts/dev check` | the raw `repo` SQL check ([below](#tests)), rustfmt, clippy `-D warnings`, the UI type check (`svelte-check`) and the UI unit tests |
| `scripts/dev fmt` | `cargo fmt --all` |
| `scripts/dev psql [db]` | psql on the dev database (also on `localhost:55432`, user and password `sourcegull`) |
| `scripts/dev test-executor` | the executor tests against the host's Docker daemon (runs `alpine` containers) |
| `scripts/dev test-batches-steps` | the batch step runner tests with real containers |
| `scripts/dev test-batch-cli` | the `gull bulk` tests with real containers against an in-process server |
| `scripts/dev test-s3 [down]` | the S3 blob store tests against MinIO |
| `scripts/dev test-kubernetes [down]` | leader election (`Lease`) and the executor's Kubernetes runtime against k3s in Docker (container `sourcegull-k3s`) |
| `scripts/dev test-codeintel` | end to end: the dev GitLab with a runner, the Helm chart in k3s, CI uploads and auto-indexing for Go, TypeScript, Java, Python and Rust (`scripts/test/codeintel.sh`; `up`, `run`, `verify`, `down`) |
| `scripts/dev test-batches-scale` | end to end: a 1,000-repository bulk change against GitLab CE, server-side and with `gull bulk apply` (`scripts/test/batches-scale.sh`) |
| `scripts/dev test-upgrade` | end to end: upgrade from an older version with Docker Compose and with the Helm chart on kind, checking data and out-of-band migrations, and the previous release on the new schema ([migrations](#migrations), `scripts/test/upgrade.sh`) |
| `scripts/dev e2e` | browser checks of the UI with Playwright ([below](#browser-checks)) |
| `scripts/dev corpus <cmd> [name]` | benchmark corpora ([below](#benchmarks)) |
| `scripts/dev bench-search --corpus <name>` | the end-to-end search benchmark |
| `scripts/dev app` | build the production image and run it on :7081 (database `sourcegull_app`) |
| `scripts/dev gitlab up\|down\|seed\|logs\|test` | the [development GitLab](#development-gitlab) |
| `scripts/dev gitea up\|down\|logs` | a fresh, empty Gitea for the [Gitea browser check](#browser-checks) |
| `scripts/dev prune [days]` | delete build outputs not written for `days` days (default 7) |
| `scripts/dev reset` | delete the database, data and build caches (and the dev GitLab) |

Cargo's cache, `target/` and `ui/node_modules` live in Docker volumes, separate from any
toolchain on the host; files the containers write into the checkout belong to your user. Cargo
never deletes old builds from `target/`, so run `scripts/dev prune` now and then. Ports
can be changed with `SOURCEGULL_HTTP_PORT`, `SOURCEGULL_UI_PORT`, `SOURCEGULL_INTERNAL_PORT`,
`SOURCEGULL_PG_PORT` and `SOURCEGULL_APP_PORT` (in the environment or a `.env` file).

### Development GitLab

Tests use a mock GitLab API (`sourcegull_codehost_gitlab::testing`). To try Sourcegull against a real
GitLab, or to check that the mock still matches GitLab, `scripts/dev gitlab up` starts GitLab CE
(Compose profile `gitlab`, about 3 GB of memory, 3–5 minutes on the first start) and seeds it
with `deploy/docker/dev/gitlab-seed.rb`. Seeding is idempotent and runs again with
`scripts/dev gitlab seed`. `scripts/dev gitlab down` stops it; its data is kept.

- URL `http://gitlab.localhost:8929` from the browser and the containers. Tools on the host
  need `127.0.0.1 gitlab.localhost` in `/etc/hosts`: GitLab's links (pagination, clone URLs)
  use that name. Root password `Xq7v-Lm2k-Pz9w`.
- Users `alice`, `bob`, `carol` (password `Hn4t-Rb8c-Wy3j`). Tokens are
  `glpat-sourcegull-dev-root` (api, sudo) and `glpat-sourcegull-dev-<user>` (read_api).
- Projects under `acme` (private, internal, empty, archived, a subgroup) and `community`
  (public), with memberships for testing permissions; see the seed file.
- OAuth application `sourcegull-dev` / `sourcegull-dev-secret` with callbacks on :7080 and :5173.

A code host connection for the `sourcegull` container:

```json
{
  "url": "http://gitlab.localhost:8929",
  "token": "glpat-sourcegull-dev-root",
  "projectQuery": ["groups/acme/projects?include_subgroups=true", "groups/community/projects"]
}
```

`scripts/dev gitlab test` runs the live tests (`crates/codehost-gitlab/tests/live.rs`) against
it. They do nothing unless `SOURCEGULL_TEST_GITLAB_URL` is set. These are fixed development
credentials; never expose this GitLab outside your machine.

### VS Code

"Dev Containers: Reopen in Container" opens the checkout in the `dev` service
(`.devcontainer/`), with rust-analyzer and the Svelte extension, and the dev Postgres next to it.

## On the host

With a Rust toolchain, Node.js and PostgreSQL 18 installed:

```sh
# Postgres for development and tests (prints the URL)
export SOURCEGULL_TEST_DATABASE_URL=$(scripts/dev-postgres.sh)

cargo test --workspace
(cd ui && npm ci && npm run build)        # optional: embed the UI

export SOURCEGULL_DATABASE_URL=$SOURCEGULL_TEST_DATABASE_URL
cargo run -p sourcegull -- migrate up
cargo run -p sourcegull -- run --roles=all    # UI and API on :7080, metrics on :6060
```

## Tests

- Unit and integration tests live with each crate. Tests that need a database use
  `SOURCEGULL_TEST_DATABASE_URL` (set by `scripts/dev test`), create a private database each, and
  pass without doing anything when it is unset. `scripts/dev test` also sets
  `SOURCEGULL_REQUIRE_TEST_DATABASE=1`, which makes them fail instead, so a green run means every
  test ran; an unreachable database always fails them.
- Tests that need outside services (Docker, MinIO, k3s, a real GitLab) have their own
  `scripts/dev test-*` command and are skipped otherwise.
- UI: `npm test` (Vitest) and `npm run check` (`svelte-check`) in `ui/`, both part of
  `scripts/dev check`. `ui/src/lib/graphql-types.ts` holds the result and variable types of
  every GraphQL operation the UI exports (`<Name>Query`, `<Name>Mutation`, `…Variables`),
  generated from `ui/schema.graphql`; `npm test` fails when it is stale, and
  `npm run graphql-types` rewrites it. Operation names must be unique. Operations are exported
  string constants in `ui/src/lib/**/*.ts` (not `.svelte` files, which the generator does not
  read), and every `graphql<…>()` call takes its generated result type (or an alias of it) rather
  than a hand-written shape.
- Repository permissions: reads of `repo` rows for a user must apply the viewer's `RepoAccess`.
  Clippy rejects the unfiltered `RepoStore` methods (`clippy.toml`), and
  `scripts/check-repo-sql.py` (first step of `scripts/dev check`, also runnable on its own)
  rejects SQL with `FROM repo` or `JOIN repo` outside `crates/db`. Integration tests
  (`crates/*/tests/`) are exempt; internal code that acts for no user (repo nodes, workers,
  metrics) and site-admin-only reads are listed in `scripts/repo-sql-allow.txt` with a reason.
  Reads shown to a viewer go in a `RepoStore` method that takes the `RepoAccess`.
- Deployment checks: `scripts/test/helm-ct.sh` (Helm lint, template, `ct install --upgrade` on
  kind) and `scripts/test/check-monitoring.sh` (Prometheus rules and dashboards).

### Browser checks

`ui/e2e/*.cjs` are Playwright scripts that drive a real Sourcegull: search, admin pages, code
navigation, code graph, policies, auto-indexing, executor secrets, bulk changes and specs,
code host connections, repositories, trends, analytics and updates.
`scripts/test/e2e-ui.sh` builds the binary and the UI, creates a fresh database, starts
`sourcegull run --roles=all` on port 7090 with one repository (a mirror of this checkout, no GitLab
needed) and runs the checks. `scripts/dev e2e` runs it in Docker with nothing installed;
screenshots go to `.e2e-out/`. `CHECKS="search admin"` runs a subset.

`CHECKS=gitea scripts/dev e2e` runs `ui/e2e/gitea.cjs` against a real Gitea (compose service
`gitea`, profile `gitea`, a pinned `gitea/gitea` image with SQLite). It is not in the default
run. `scripts/dev e2e` first recreates Gitea empty with `scripts/dev gitea up`, which adds the
site administrator `root` (password `Jk5r-Tw2q-Vb7n`); the check then seeds it through the API
(two users, a private repository only one of them can read, an OAuth2 application), adds a
Gitea connection with `authorization: {identityProvider: {type: "oauth"}}` and a `gitea`
sign-in provider, signs in as each user through Gitea, and checks search, the file page and
MCP `read_file` for each, and that removing the collaborator on Gitea followed by a
permissions sync revokes access. Gitea answers as `http://gitea:3000` inside the containers and
on `127.0.0.1:3000` (`SOURCEGULL_GITEA_PORT`) on the host; `scripts/dev gitea down` removes it.

## Benchmarks

Search performance is measured end to end, as a user sees it: from the HTTP request to the last
streamed result.

- **Corpora** (`scripts/bench/corpus.py`, `scripts/dev corpus list|fetch|verify|path <name>`):
  `tiny`, `small-repos` (20,000 small repositories), `gitlab-dist`, `linux`, `linux-full` and
  `mixed-10g`. Public repositories are pinned to a tag and commit; synthetic ones are generated
  deterministically, and every corpus is checked against a manifest. They live under
  `$SOURCEGULL_CORPUS_DIR` (default `~/.cache/sourcegull-corpora`).
- **Search** (`scripts/dev bench-search --corpus <name>`, `scripts/bench/search.sh`): builds
  Sourcegull (`PROFILE=release` for an optimized build), indexes the corpus into a private database,
  and runs the corpus's query set against `/.api/search/stream`, reporting latency percentiles
  per query class (selective, common, regexp, unindexed, …). `OUT=<file>` keeps the raw JSON.
- **Index build** (`scripts/dev scripts/bench/index-build.sh [threads] [reps]`): build
  throughput on pinned corpora (a Go release, a synthetic 100 MB tree, and the Linux kernel with
  `LARGE=1`), with a phase breakdown and full builds with and without symbols.

Compare runs on the same machine; numbers from different hosts are not comparable.

## Migrations

During a rolling upgrade the previous release runs against the new schema, so a change is made
in steps (expand, migrate, contract): add the new column or value, move the code to it, and
remove the old column or value one release after the code stopped using it, never in the
migration that replaces it. `scripts/dev test-upgrade` checks this in two ways:

- `scripts/test/upgrade.sh contract` applies the previous release's migrations and this
  checkout's to scratch databases and fails when a column, table or value allowed by a CHECK
  constraint is gone while the previous release's code still names it with its table. It needs
  only Docker and runs in seconds. A removal it flags wrongly (a name left in a comment) goes in
  `scripts/test/upgrade/contract-allow.txt`.
- After the upgrade to this checkout, the Compose test starts the previous release's image on
  the new schema, without running its migrations, and runs the same data and search checks.

The previous release is the newest release tag, else the commit the branch started from;
`SOURCEGULL_UPGRADE_PREVIOUS` picks another ref. The Compose and Helm upgrade tests start from
the second newest release tag (the newest while there is only one; `SOURCEGULL_UPGRADE_FROM`
picks another ref) and skip until the first release is tagged.

## Resource limits

Local builds and tests are capped so they cannot exhaust the host: the dev containers
(`compose.yaml`) get `SOURCEGULL_DEV_CPUS` (4) cores and `SOURCEGULL_DEV_MEMORY` (8g) without swap,
and every local image build goes through `scripts/image-build.sh`, a buildx builder with the same
kind of limits, 3 rustc jobs and thin LTO. A build that needs more fails with an out-of-memory
error; raise the limit in `.env` (and `docker buildx rm sourcegull-limited` for the builder)
instead of building without it. The release workflow builds the published images the same way
(thin LTO, 16 codegen units), within the runner's memory.

## Secret scanning

`scripts/gitleaks.sh` runs gitleaks in Docker with `.gitleaks.toml`: the default rules, private
IPv4 addresses (use documentation addresses such as `10.0.0.1` or `192.168.1.10` in examples
and tests), and an allowlist of the test fixtures. Install the pre-commit hook once per clone:

```sh
git config core.hooksPath scripts/hooks
```

Names that must not be published, such as internal host names, go as `[[rules]]` in
`.git/info/gitleaks.toml`, which every worktree shares and which is never committed; the script
adds them to `.gitleaks.toml`. The release workflow
scans the whole history before it builds.

## Releases

Set the version in `Cargo.toml` (`[workspace.package]`, `X.Y.Z-rc.1` for a pre-release), the
image defaults (`SOURCEGULL_VERSION` in `deploy/docker/compose.yaml` and `.env.example`,
`ARG VERSION` in the Dockerfile) and the chart's `appVersion`, merge to `main`, then run
**Actions → Release** on `main` with **publish** checked. `.github/workflows/release.yml` runs on
the self-hosted runner (labels `self-hosted`, `linux`): it scans the history for secrets, checks
the chart (`scripts/test/helm-ct.sh template`), builds
the linux/amd64 images, runs `scripts/test/release-smoke.sh` and the Gitea browser check against
the runtime image (`SOURCEGULL_E2E_IMAGE=<image> CHECKS=gitea scripts/dev e2e`),
then pushes `ghcr.io/sourcegull/sourcegull` and `ghcr.io/sourcegull/gull`, pushes the Helm chart to
`oci://ghcr.io/sourcegull/charts` when its `version` is not published yet (bump it with any chart
change, see the chart README), signs them, and creates the `vX.Y.Z` tag and the GitHub release
with the Compose bundle (`sourcegull-compose.tar.gz`, the files of `deploy/docker/` a host needs)
attached. Without **publish** it builds and tests only.

Builds reuse a persistent builder on the runner (`sourcegull-release`): its state volume keeps the
image layers and the cache mounts (cargo's `target/`, named after the Rust version, the crate
sources and npm's cache), so a release recompiles only the crates that changed. Garbage
collection (`.github/buildkitd.toml`) keeps the store under 30 GB, least recently used first.

The runner accepts only this repository's workflows from `main` (the organization's runner
group lists them), so workflows added or changed on other branches or in pull requests cannot
run on it; the other workflows also run by hand, from `main`.

## Conventions

- Run `scripts/dev check` before committing: `scripts/check-repo-sql.py`, `cargo fmt --all
  --check`, `scripts/check-test-modules.sh`, `cargo clippy --workspace --all-targets -- -D
  warnings`, `svelte-check` and the UI tests must pass.
- A crate with several integration test files builds them as one binary, `integration`
  (`tests/main.rs` declares each file as a module; the crate sets `autotests = false`). Declare
  a new file there, and run one file's tests with `cargo test -p <crate> --test integration
  <file>::`. Dev builds keep line tables only, which keeps the test binaries small.
- Keep `Cargo.lock` and `ui/package-lock.json` in sync with the manifests (`--locked` builds).
- Never edit an applied migration in `crates/db/migrations/`: migrations are checksummed. Add a
  new one, keeping the schema of the previous release working ([migrations](#migrations)).
- Comments explain what code does and why; public items have doc comments.
- Dashboards and alerts are generated: change `crates/monitoring` and run
  `sourcegull monitoring generate --repo .` to update `deploy/` and `docs/observability/`.
- Metric names are constants in `sourcegull_types::metric_names`, used by the code that records
  them and checked against every dashboard query (`crates/monitoring/tests/generated.rs`); a
  new metric gets a constant there, not a string literal.

## GraphQL by domain: adding or moving a domain

The GraphQL API is split so that editing one domain's resolvers rebuilds that domain's crate,
the schema assembly and the binary, not the rest of the API:

- `crates/graphql-core` (`sourcegull-graphql-core`) holds what every resolver needs: `context`
  (`Services`, `Viewer`, `codes`, `error`, `access_error`, `db_error`, `require_viewer`,
  `require_site_admin`, `repo_access`, `page_size`, `to_int`, `validation`), `pagination`
  (`PageInfo`, `ConnectionPageInfo`, `PageArgs`, cursors), `ids` (the node kinds and ID parsers of
  users, organizations, namespaces and repositories), `events::log_backend`,
  `repo::{require_reader, load_readable}`, `EmptyResponse`, `declared` (the read-only guards of
  objects the configuration-as-code files own), `git::{git_error, BLOB_CONTENT_MAX_BYTES}`,
  `execution::ExecutionLogEntryNode` (executor steps), `audit::record` (security events), the
  value types several domains return (`diff::{DiffStat, FileDiffHunk}`,
  `codehost::{ExternalServiceKindGql, ExternalLink}`), the `Node` interface and the node loader
  registry (`node`), a feature's part of the schema (`feature::GraphqlFeature`) and the
  late-bound references to shared types (`shared`). It depends on no domain crate.
- A domain's GraphQL lives in `crates/<domain>-graphql` (`sourcegull-<domain>-graphql`, for
  example `crates/insights-graphql`). It depends on `sourcegull-graphql-core` and its domain crate,
  never on `sourcegull-graphql` or another domain's GraphQL crate. A separate crate rather than a
  `graphql` module in the domain crate keeps the engine (used by workers and HTTP routes) out of
  the rebuild.
- `crates/graphql` (`sourcegull-graphql`) assembles the schema: the shared types (`User`, `Org`,
  `Repository`, git types), `node(id:)`, the core domains, and the merged `QueryRoot` /
  `MutationRoot`, which are generic over the feature list: the core fields, then the
  features'. `build_schema::<F>`, `sdl::<F>` and `routes::<F>` take the list. Its
  `crate::context` and `crate::pagination` are re-exports of `sourcegull-graphql-core`, so its
  modules import them as before.
- `crates/sourcegull/src/features.rs` lists the features: `all()` (routes, workers, singletons:
  `sourcegull_feature::Feature`) and `Graphql`, a tuple of the features' `GraphqlFeature`s, in the
  order of their root fields. `graphql_schema`, `graphql_routes`, `graphql_sdl` and
  `install_auto_applier` build with that list; the server and every test that needs the whole
  schema (`crates/graphql/tests`, the executor, CLI and search jobs tests, which have `sourcegull`
  as a dev-dependency) use them.

**Adding a feature.** Write the domain crate (`crates/<feature>`, its store and engine) and, if
it has an API, `crates/<feature>-graphql`. In the domain crate, implement
`sourcegull_feature::Feature` for its routes, queue workers and singletons; in the GraphQL crate,
implement `sourcegull_graphql_core::feature::GraphqlFeature` on a unit struct (by convention in
`src/feature.rs`, for example `sourcegull_monitors_graphql::MonitorsGraphql`):

- `type Query` / `type Mutation`: the feature's root objects; several objects are merged with
  `#[derive(MergedObject, Default)]` into one (`OwnFeatureQuery(TeamsQuery, OwnQuery)`; give it
  a name no GraphQL type has), and `MergedObjectTail` stands for none;
- `names()`: the feature's name;
- `register_types`: `register_node::<T>(registry)` for each object type that implements `Node`
  (and any type no root field reaches);
- `node_loaders`: `registry.add(node_loaders::<Node>())`, the loaders of its node kinds, with
  `impl From<DomainNode> for Node` (`Node::new(object)` for each variant).

Then add one line to each list in `crates/sourcegull/src/features.rs` (`all()` and `Graphql`), the
crates to the root `Cargo.toml` and to `crates/sourcegull/Cargo.toml`, and extend the expected names
and node kinds in `crates/sourcegull/tests/features.rs`. Nothing in `crates/graphql`,
`server.rs` or `worker.rs` changes. Fields the feature adds to a shared type (`User.monitors`)
are reverse fields (below) and do go into `crates/graphql`. A feature's root fields come after
the core ones, in list order, so a new feature adds a block at the end of `QueryRoot` /
`MutationRoot` in `ui/schema.graphql`.

**Moving a domain.** `git mv crates/graphql/src/<domain>* crates/<domain>-graphql/src/`, add
the crate to the root `Cargo.toml` (`[workspace.dependencies]`) and as a dependency of
`sourcegull-graphql`, and put `use sourcegull_graphql_core::{context, pagination};` at the crate root so
`crate::context::…` paths keep working. Then replace every remaining `crate::` path into
`sourcegull-graphql`: helpers come from `sourcegull-graphql-core` (move a helper there when two crates
need it, and re-export it from its old place), shared types become references (below). A
`RepositoryNode` that a module only carries as data becomes a `sourcegull_types::Repo` (loaded with
`repo::load_readable`), and a single git read needs no `RepoHandle`:
`GitReader::new(services(ctx)?.repo_clients.clone(), &repo)`. Remove the module's query and
mutation objects from `QueryRoot` / `MutationRoot` and its node types from `node.rs`, and
register the crate as a feature (above); its root fields move to the features' block, so
regenerate `ui/schema.graphql` (`SOURCEGULL_UPDATE_SCHEMA=1 cargo test -p sourcegull-graphql --test
schema_file`) and the UI types (`npm run graphql-types`), and check that only field order
changed. Integration tests stay in `crates/graphql/tests`,
since they need the whole schema. If a moved file is listed in
`scripts/repo-sql-allow.txt`, update its path. Code that runs queries on the built schema
(`batches::auto_apply`) stays in `sourcegull-graphql`, in a module that re-exports the domain
crate (`pub use sourcegull_batches_graphql::*;`) so its old paths keep working.

**Returning a shared type.** A domain crate cannot name `UserNode`, so it returns
`sourcegull_graphql_core::shared::UserRef(user)` (also `OrgRef`, `RepositoryRef`), holding the
loaded row. A reference is an output type with the real type's GraphQL name; it registers
nothing, and when resolved it hands the row to the `SharedTypes` resolver that `sourcegull-graphql`
installs as schema data, which builds the full type and resolves the selection on it. The
schema is unchanged: `sourcegull-graphql` registers the real types in `shared::register`, so each
name has one definition. To add a reference (for example `NodeRef` or a git type), add a
`shared_ref!` line and a `SharedTypes` method in `crates/graphql-core/src/shared.rs`, implement
it in `crates/graphql/src/shared.rs` and register the real type there. Check with
`cargo test -p sourcegull-graphql --test integration schema_file::`: `ui/schema.graphql` must not change.

An interface whose implementations are shared types (`Namespace` = `User` | `Org`) gets a
reference the same way: `NamespaceRef` holds a `NamespaceValue` enum of the loaded rows, and
the assembly resolves it on the real interface (`NamespaceNode`). A reference cannot be a
variant of a union or interface that a domain crate derives (the derives need the variant to
be a real object), so such a union or interface stays in the assembly and the domain crate
returns a reference to it (bulk change previews return `File2Ref`, whose `File2Data` is a
`GitBlobData` or a `VirtualFileData` with its content loader; `File2` and `VirtualFile` are in
`crates/graphql/src/file2.rs`). A union that mixes a shared type and a domain type (`Owner` =
`Person` | `Team`) cannot stay in the assembly as a whole reference, since `sourcegull-graphql-core`
cannot name the domain type; the domain crate implements it by hand instead
(`sourcegull_own_graphql::ownership::Owner`): `create_type_info` registers a `MetaType::Union`
with the members' names, `introspection_type_name` names the member, and `resolve` hands the
union's selection set to the member (`PersonRef` or `TeamNode`), which handles `__typename`
and fragments on either member as a derived union does. Loaders that build a reference
go in `sourcegull-graphql-core` (`shared::load_namespace`); the assembly maps their value to its
own type.

A domain that works on a git location does not take the assembly's git types: it takes the
plain parts it needs, and the assembly builds them from its own type. Ownership takes an
`OwnedPath` (the repository, its `GitReader`, the commit, the revision it was reached by and
the path), which `EntryBase::owned_path` builds for `GitBlob.ownership`, `GitTree.ownership`
and `GitCommit.ownership`; a CODEOWNERS file in the repository comes back as a `GitBlobData`
that keeps that revision for its URL.
Registering such an interface late (in `shared::register`) can change the order in which a
type lists its interfaces in the SDL; register it from the field that reached it before:
`User.teams` returns `UserTeams`, which registers `TeamMember` before `TeamConnection`; the
`ownership` fields return `Ownerships`, which registers `File2` before `OwnershipConnection`;
`User.executorSecrets` and `Org.executorSecrets` return `NamespaceExecutorSecrets`, which
registers `Namespace` before `ExecutorSecretConnection`.

Every reference resolves through `SharedTypes`, including the ones bulk changes and
ownership return: `Person`, `Executor` (whose type is
`sourcegull_executors_graphql::executors::ExecutorNode`), `GitRef` (`BranchRef`, a branch at a
commit), `GitBlob`, `VirtualFile` and `File2`. A reference may name a type of another domain
crate: the assembly depends on both and registers that type.

**Reverse fields.** A field on a shared type that returns a domain type (`User.monitors`,
`Repository.ingestedCodeowners`, `GitBlob.ownership`) stays on the shared type in `sourcegull-graphql` and calls a
public function of the domain's GraphQL crate that returns the domain's type; the assembly
depends on the domain crates, so this direction needs nothing else.

**Node lookup.** `Node` (`sourcegull_graphql_core::node::Node`) is a hand-written interface type,
not a derived enum: it holds any object (`Node::new(object)`), resolves the selection on it and
names it in `__typename`. Its implementations are registered at schema build:
`register_node::<T>` registers `T`, adds `implements Node` to it and adds it to the
interface's possible types. The core types (`User`, `Org`, `Repository`, …) are registered in
`crates/graphql/src/node.rs` (`CoreNodes`) and matched in `node(id:)`; a feature registers its
own in `GraphqlFeature::register_types`. Since every implementation is registered after the
roots and shared types, `Node` comes last in each type's `implements` list. The domain crate
exports an enum of its node types (`InsightsNode`), or uses its node type directly when it has
one (`MonitorNode`), and `node_loaders::<N: From<InsightsNode>>() -> [NodeLoader<N>; _]`, the
loaders of its node kinds; its `GraphqlFeature::node_loaders` adds them to the schema's
`NodeRegistry` (schema data), which `node(id:)` consults before the kinds it matches itself. A
kind registered twice panics when the schema is built.
