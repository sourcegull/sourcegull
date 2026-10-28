# Roadmap

Planned work, in the order it is expected to land. Each item lists its goal, design, the code it
touches, how it is tested and when it is done. Items stay within the
[fixed decisions](../CLAUDE.md): one binary, PostgreSQL only, GitLab, GitHub and Gitea.

| # | Item | Size | Depends on |
|---|---|---|---|
| 1 | [Published images](#1-published-images) | S | — |

Item 1 is small and unblocks simpler installs. CI runs only for release tags (item 1); every other check runs locally with `scripts/dev`.

Only work that has not shipped is listed here. When an item (or a phase of one) is done, it is
removed, and what it added is described in the user docs where needed.

---

## 1. Published images

**Goal.** Installs pull a released image instead of building one.

Today `deploy/docker/Dockerfile` builds `runtime` and `cli` targets and every install starts
with `docker buildx build`.

### Design

- **Release workflow** (`.github/workflows/release.yml`), run on `v*` tags and manually:
  - builds the `runtime` and `cli` targets for `linux/amd64` and `linux/arm64`;
  - pushes `ghcr.io/<owner>/sourcegull` and `…/gull` with the tags `X.Y.Z`, `X.Y` and `latest`
    (not `latest` for pre-releases);
  - attaches an SBOM and build provenance (`docker buildx --sbom --provenance`), and signs
    the images by digest with keyless signing;
  - runs a smoke test before pushing: start `supervise` against a Postgres service, wait for
    `/readyz`, load the sign-up page, then add a Gitea connection and wait for one search result
    (the `gitea` compose profile that `CHECKS=gitea scripts/dev e2e` uses).
- **Compose and Helm** default to the published images (`SOURCEGULL_IMAGE` in `.env.example`,
  `image.repository` in the chart). Building from source stays documented.
- CI stays manual-only for pull requests; only the release workflow runs on tags.

### Changes

- `.github/workflows/release.yml`.
- `deploy/docker/compose.yaml`, `.env.example`, `deploy/helm/sourcegull/values.yaml`.
- `docs/install.md`: Docker Compose and Helm with pulled images first, building from source
  after; `README.md` Install section without the build step.

### Tests

- The release smoke test above, on both architectures (arm64 under emulation, smoke only).
- `scripts/dev e2e` can run against a pulled image (`SOURCEGULL_E2E_IMAGE`).

### Done when

On a clean machine with Docker, the Compose stack starts from the published image without a
build, and the Helm chart installs with its default `image.repository`.

---

## Candidates

Not planned until asked for: organizations and teams, and repository-scoped code intelligence
policies, in the configuration as code resources file; teams synced from GitHub teams and Gitea
organization teams (as `own.syncTeamsFromGitLabGroups` does for GitLab groups); a default
permissions sync interval that scales with the number of private repositories; requiring
encryption keys, which would remove the action job path that watch HTTP actions use on installs
without keys (`cm_action_jobs`).
