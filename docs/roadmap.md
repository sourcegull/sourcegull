# Roadmap

Planned work, in the order it is expected to land. Each item lists its goal, design, the code it
touches, how it is tested and when it is done. Items stay within the
[fixed decisions](../CLAUDE.md): one binary, PostgreSQL only, GitLab, GitHub and Gitea.

Nothing is planned right now. CI runs on a self-hosted runner: only the release workflow
starts on its own (on `v*` tags); every other check runs locally with `scripts/dev`, or by hand.

Only work that has not shipped is listed here. When an item (or a phase of one) is done, it is
removed, and what it added is described in the user docs where needed.

---

## Candidates

Not planned until asked for: organizations and teams, and repository-scoped code intelligence
policies, in the configuration as code resources file; teams synced from GitHub teams and Gitea
organization teams (as `own.syncTeamsFromGitLabGroups` does for GitLab groups); a default
permissions sync interval that scales with the number of private repositories; requiring
encryption keys, which would remove the action job path that watch HTTP actions use on installs
without keys (`cm_action_jobs`).
