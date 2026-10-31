# Sourcegull

Self-hosted code search and code intelligence for GitLab, in Rust (SvelteKit UI).

## Where things are
- User docs: `docs/` (start at `docs/README.md`); architecture and the crate map: `docs/architecture.md`.
- Development, tests, benchmarks and conventions: `docs/development.md`.

## Fixed decisions (do not revisit without asking)
- Rust. One binary with roles (`web`, `repo`, `worker`, `blob`, `executor`, `migrate`).
- No features that depend on a vendor cloud. Install targets: Docker and Kubernetes Helm.
- Code hosts: GitLab (gitlab.com and self-managed), GitHub (github.com and GitHub Enterprise Server) and Gitea, behind code-host traits. GitLab has every feature; GitHub and Gitea cover repository sync, webhooks and sign-in.
- Search index: our own format behind the `SearchIndex` trait.
- No Redis. Caches, rate limits, sessions and pub/sub go through traits with in-process and Postgres implementations.
- Performance is measured end to end, as the user sees it.

## Conventions
- Build, test and check in Docker: `scripts/dev test`, `scripts/dev check`, `scripts/dev e2e`.
- To show a UI change in the browser, use Vite on `:5173` (hot reload, under 0.1 s), not an image
  build (~4 min); see "Seeing a UI change" in `docs/development.md`.
- Before starting a task, run `scripts/dev prune` (it prints the size of `target/` before and
  after); cargo never deletes old builds, and a full target volume breaks builds and tests.
- Never edit files under `crates/db/migrations/` that have been committed; add a new migration.
- Code comments describe the code as it is, not its history or plans. Don't name other products in code or docs; only the README names alternatives.
- Third-party code that is ported must be credited in `NOTICE`.
- `docs/roadmap.md` lists planned work only. When an item or phase is done, remove it from the roadmap (no "done" markers) and document what it added in the relevant user doc where needed.
