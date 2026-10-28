# Sourcegull documentation

Sourcegull is a self-hosted code search and code intelligence platform for GitLab, GitHub and
Gitea. It runs as one binary with PostgreSQL as its only dependency, and installs with Docker
or a Helm chart.

## Running Sourcegull

- [Install](install.md): Docker Compose, Kubernetes (Helm), S3 blob
  storage, first sign-in and connecting GitLab, GitHub or Gitea.
- [Configuration](configuration.md): environment variables, site configuration, user and
  organization settings, encryption keys, database pools, configuration as code (resources
  and global settings files).
- [Permissions and sign-in](permissions.md): sign-in providers, repository permissions from
  GitLab, roles, access tokens, access requests, SCIM and audit events.
- [Monitoring](observability/): Grafana dashboards and Prometheus alerts, with the next steps
  for each alert ([alerts.md](observability/alerts.md)).
- Upgrades: [Docker Compose](../deploy/docker/upgrade.md), [Helm chart](../deploy/helm/sourcegull/README.md).

## Using Sourcegull

- [Search](search.md): the query language, search contexts, saved searches and search jobs.
- [Code navigation](code-navigation.md): precise (SCIP), syntactic and search-based navigation,
  auto-indexing and file ranking.
- [Bulk changes](bulk-changes.md): bulk change specs, running them on executors or locally, and
  merge requests on GitLab.
- [Other features](features.md): rendered Markdown and READMEs, trends, watches, notebooks,
  code ownership, compute, usage analytics, outbound webhooks and site administration.
- [AI agents (MCP)](mcp.md): the MCP server at `/.api/mcp` and how to connect coding agents.

## Working on Sourcegull

- [Architecture](architecture.md): roles, storage, gRPC, repo node partitioning, the search
  index and a map of the crates.
- [Development](development.md): the Docker development environment, tests, browser checks,
  benchmarks and conventions.
- [Roadmap](roadmap.md): planned work, with the design and the definition of done for each item.
