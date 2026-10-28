# Code navigation

When you view a file, Sourcegull offers hover information, **go to definition**, **find
references** and **find implementations**. Three tiers answer, best first; the first tier with
data for the position answers (precise references are complemented, see below), and each result
shows where it came from:

| Tier | Source | Precision | Setup |
|---|---|---|---|
| Precise | SCIP indexes from a compiler-accurate indexer | exact, across repositories | upload indexes from CI, or auto-indexing |
| Syntactic | tree-sitter scopes in the file | exact for local variables and parameters | none |
| Search-based | symbol and text search | heuristic | none |

Precise references are complemented for repositories without precise data. When the symbol
can be used from other repositories (it is not local to its file), Sourcegull also looks for
repositories that mention the symbol's package (for Go, a `go.mod` that requires the module)
but have no completed index serving their default branch, because they were never indexed or
their index failed. Search-based references from those repositories follow the precise ones;
for Go only files that import the symbol's package count. Each reference keeps its provenance,
and the references panel and API say which repositories answered precisely and which
search-based, and why. This lookup is limited to 20 repositories and 3 seconds; when either
limit is reached, the answer says so.

All navigation runs with the viewer's repository permissions.

## Precise navigation (SCIP)

Precise navigation reads SCIP indexes produced by
language-specific indexers (`scip-go`, `scip-typescript`, `scip-java`, `scip-python`,
`scip-ruby`, `scip-dotnet`, `scip-clang`, rust-analyzer, …). An index belongs to one commit of
one repository. Workers process each upload into a code graph file in the blob store and record
which symbols and packages it defines and uses, so references resolve across repositories.

For a commit with no index of its own, Sourcegull uses the nearest indexed commit and adjusts
positions for the lines that changed in between.

A symbol from another repository is matched by its package and version, as the indexer names
it (for Go, the module path and the version required in `go.mod`). When a library is indexed at
one version and its users import others (the library is indexed at its default branch while
services pin older releases), navigation links them across versions:

- **Go to definition**: when no index defines the symbol at the version the file uses, the
  definition comes from an index of the same package at another version, preferring the index
  serving the library's default branch, else the newest.
- **Find references**: references from indexes that hold the symbol at other versions of the
  package are added after the exact matches.

Exact versions always win, and at most 20 other versions are looked at. Results linked across
versions are marked with both versions: the definition and each such reference carry
`crossVersion { requested found }` in the API, and the references coverage line lists the
repositories reached that way, e.g. `precise: 5 repositories · other versions: billing
(v1.0.54) · search-based: none`. A symbol that was renamed or moved between the versions is not
linked.

The uploads, their processing state and the index configuration of a repository are on its
**Code graph** page (`/<repo>/-/code-graph`).

### Uploading from GitLab CI

[`deploy/gitlab-ci/sourcegull-scip.gitlab-ci.yml`](../deploy/gitlab-ci/sourcegull-scip.gitlab-ci.yml)
is a CI template that indexes a project and uploads the result. Copy it into a project on your
GitLab instance and include it from each project's `.gitlab-ci.yml`:

```yaml
include:
  - project: infra/sourcegull-ci
    file: /sourcegull-scip.gitlab-ci.yml
```

A copy committed to the project itself works too (`include: - local: /ci/sourcegull-scip.gitlab-ci.yml`).

Then set two CI/CD variables:

- `SOURCEGULL_ENDPOINT`, the Sourcegull URL (e.g. `https://sourcegull.example.com`);
- `SOURCEGULL_CLI_IMAGE`, an image with the `gull` client. Build it from the `cli` target of
  [`deploy/docker/Dockerfile`](../deploy/docker/Dockerfile)
  (`docker build -f deploy/docker/Dockerfile --target cli -t registry.example.com/sourcegull-cli:<version> .`)
  and push it to a registry your runners can pull from. The template has no default: without
  it, the index and upload jobs are skipped and the job `sourcegull-scip-missing-cli-image` fails
  (allowed to fail) with a message saying what to set. (GitLab does not accept an empty
  `image:`, so the template skips the jobs by rule instead.)

Uploads authenticate with either:

- `SOURCEGULL_ACCESS_TOKEN`, a masked variable holding an [access token](permissions.md#access-tokens)
  of a user who can read the repository, or
- the job's own `CI_JOB_TOKEN`, when the site configuration has
  `"codeIntel.gitlabJobTokenUploads": true`; Sourcegull checks the token with GitLab and accepts
  the upload only for the job's own project. The upload records the GitLab job id, its ref
  and the username of the user who triggered the job (the `gitlabCiJob` field of a precise
  index in the API), and only that job can add parts to a multipart upload it started.

The repository name is derived from `CI_PROJECT_URL` (`gitlab.example.com/group/project`). If
the code host connection uses a custom repository path pattern, set
`SOURCEGULL_UPLOAD_FLAGS: "-repo=<name>"` with the name as Sourcegull shows it.

The template has an index job and an upload job per language: Go (`go.mod`), TypeScript and
JavaScript (`tsconfig.json`, `package.json`), Java/Scala/Kotlin (`pom.xml`, `build.gradle`,
`build.sbt`), Python (`pyproject.toml`, `setup.py`, `requirements.txt`) and Rust
(`Cargo.toml`; the cargo registry is cached per project). A language's jobs run when its build
file is at the repository root. Per language (`<LANG>` = `GO`, `TYPESCRIPT`, `JAVA`, `PYTHON`,
`RUST`):

| Variable | Effect |
|---|---|
| `SOURCEGULL_<LANG>_ENABLED: "true"` | run even without the build file at the root |
| `SOURCEGULL_<LANG>_DISABLED: "true"` | never run |
| `SOURCEGULL_<LANG>_ROOT` | index a subdirectory |
| `SOURCEGULL_<LANG>_IMAGE` | indexer image (default: the image auto-indexing uses, pinned by digest) |
| `SOURCEGULL_<LANG>_INDEX_FLAGS` | extra indexer arguments |
| `SOURCEGULL_UPLOAD_FLAGS` | extra `gull code-intel upload` flags, e.g. `-repo=<name>` |
| `SOURCEGULL_SCIP_MERGE_REQUESTS: "true"` | also run in merge request pipelines |
| `SOURCEGULL_SCIP_DISABLED: "true"` | turn everything off |

The jobs are in the `test` stage and allowed to fail; redefine a job to change anything else.
A project whose `.gitlab-ci.yml` defines its own `stages:` must list `test` there, or set
`stage:` on each template job it uses:

```yaml
sourcegull-scip-go:
  stage: build
sourcegull-scip-go-upload:
  stage: build
```

Otherwise GitLab refuses to create the pipeline (the job's stage does not exist).

Because the jobs may fail without failing the pipeline, check that uploads arrive: after a
pipeline runs, the repository's **Code graph** page in Sourcegull
(`/<repo>/-/code-graph`) lists the upload and its processing state, and the upload job's log
ends with a link to that page. A failed upload job shows the reason in its log (for example,
an unknown repository: check `-repo`).

### Uploading with `gull`

`gull code-intel upload` uploads an index from any machine or CI system:

```sh
export SOURCEGULL_ENDPOINT=https://sourcegull.example.com SOURCEGULL_ACCESS_TOKEN=...
gull code-intel upload                      # ./index.scip; repository, commit and root from git
gull code-intel upload -repo=gitlab.example.com/acme/api -commit=<sha> -root=services/api -file=index.scip
```

In GitLab CI, the repository and commit come from the job's `CI_*` variables, and
`CI_JOB_TOKEN` is used when no access token is set. `gull code-intel upload --help` lists every
flag. The endpoint is `POST /.api/scip/upload`.

Uploads larger than `codeIntel.maxUploadBytes` (default 2 GiB, counted as sent, so usually
gzip-compressed, and summed over the parts of a multipart upload) are refused with
`413 Payload Too Large` and discarded. Compressed SCIP indexes of large repositories are
typically tens to a few hundred MB.

## Auto-indexing

Instead of (or in addition to) CI uploads, Sourcegull can build indexes itself on
[executors](architecture.md#executors):

1. Run at least one executor with the `codeintel` queue (Compose profile `executors`, or Helm
   `executor.enabled=true`), and set the same secret in `executors.accessToken` and
   `SOURCEGULL_EXECUTOR_TOKEN`.
2. Set `"codeIntelAutoIndexing.enabled": true` in the site configuration.
3. Create a [policy](#policies) with indexing enabled for the repositories and branches to
   index.

For each commit a policy names, Sourcegull infers the index jobs from the repository's files. The
built-in recognizers cover Go, Java/Scala/Kotlin, Python, Ruby, Rust, TypeScript/JavaScript,
C#/.NET and C/C++, using pinned indexer images (override them per language with
`codeIntelAutoIndexing.indexerMap`, e.g. for a registry mirror). The jobs run on executors and
upload their result like a CI job. A job checks out only its commit, together with the tags
that point at it, so an indexer that reads the version from a tag (such as a Go module's
`v1.2.3`) gives a library's index the release version its dependents import.

Indexers download the project's dependencies, so private ones need credentials, stored as
executor secrets with the Code intelligence scope (Site admin → Executor
secrets); a job receives the ones its recognizer asks for:

- Go: `GOPRIVATE` and `GONOSUMDB` (the GitLab hosts, comma-separated, e.g.
  `gitlab.example.com`), and `NETRC_DATA`, written to `~/.netrc` in the job, with one line per
  host: `machine gitlab.example.com login oauth2 password <token>`, using a token with
  `read_repository`. Without them, `go mod download` fails for every module that requires a
  private one, and GitLab answers unauthenticated `go get` requests for private projects with
  the wrong repository path. `GOPROXY`, `GONOPROXY` and `GOSUMDB` are passed through as well.
- TypeScript/JavaScript: `NPM_TOKEN` for a private registry.

A job's log on the repository's Code graph page shows which dependency failed; the
[coverage](#coverage) page lists the repositories whose indexing fails.

To control the jobs for a repository:

- a `sourcegull.yaml` file at the repository root, or a configuration saved on the repository's
  Code graph page, lists the index jobs explicitly (steps, indexer image, arguments, root);
- the **inference script** (Site admin → Inference script) is a Lua script that can add
  recognizers or turn built-in ones off for the whole instance. It runs in a sandbox with an
  instruction budget, a deadline and a memory limit, and the page can preview the jobs it
  infers for a repository.

Other settings: `codeIntelAutoIndexing.allowGlobalPolicies` (let policies without repository
patterns enable indexing everywhere), `codeIntelAutoIndexing.policyRepositoryMatchLimit` and
`codeIntelAutoIndexing.repositoryCommitsPerHour`.

### Coverage

Site admin → Index coverage shows which repositories have a current precise index. Each
repository is in one state:

- **Up to date**: a completed index is at the default branch tip, or the newest completed index
  is younger than the stale window (7 days by default, adjustable on the page);
- **Stale**: completed indexes exist, none at the tip, and the newest is older than the window;
- **Failing**: the latest index job or upload failed after the newest completed index;
- **Not indexed**: an indexing policy applies, but no index has completed;
- **Not targeted**: no indexing policy applies (always the case while auto-indexing is off) and
  nothing was uploaded.

The table can be filtered by state and name, and shows the last indexed commit and its age, the
latest failure message and a link to the repository's Code graph page, where the job's logs
are. Navigation in a stale or failing repository answers from older data or from search, so
references into it can be missing. The worker exports the same counts as
`sourcegull_codeintel_coverage_repos{state}` (refreshed every 5 minutes), and the
`worker_codeintel_failing_repos_growth` [alert](observability/alerts.md) fires when more
repositories are failing than 6 hours before.

## Policies

Code graph policies (Site admin → Code graph policies, and per repository) decide which
precise indexes are kept and which commits are auto-indexed. A policy names:

- the repositories it applies to (one repository, name patterns, or all),
- the git objects: a branch or tag name pattern, or one commit,
- **retention**: how long indexes visible from those objects are kept, and whether
  intermediate commits count,
- **indexing**: whether to auto-index them, the maximum commit age, and whether intermediate
  commits are indexed.

Indexes that no policy retains expire and are removed in the background (unless another index
depends on them). Indexes that serve the tip of a repository's default branch are always kept.

## Syntactic navigation

For Go, Rust, Python, Java, JavaScript, TypeScript, C and C++, Sourcegull parses the file with
tree-sitter and resolves local variables, parameters and other names defined inside functions
and blocks to their definitions. Names defined at the top level of a file, which other files
can use, are left to the other tiers. It needs no setup and works on any revision.

## Search-based navigation

When no precise data covers a position, Sourcegull searches for the symbol: definitions come from
symbol search (tree-sitter symbol extraction for 19 languages), references from a
case-sensitive whole-word text search, both narrowed to the file's language family. Hover text
comes from the definition's line and its doc comment.
Results are labelled as search-based, since a name may match unrelated code.

## Ranking

A worker job builds the reference graph of all precise indexes that serve each repository's
default branch: files are nodes, and references between them (also across repositories) are
weighted edges. It computes PageRank over this graph and stores a static rank per file. Repo
nodes write the ranks into the search index at the next full build, and search uses them to
break ties between equally good matches, so central, widely used files come first. The ranking
is on by default (`codeIntelRanking.documentReferenceCountsEnabled`) and has no effect until
precise indexes exist.
