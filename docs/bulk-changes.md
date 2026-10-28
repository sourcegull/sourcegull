# Bulk changes

A bulk change makes the same change across many repositories and tracks the resulting GitLab
merge requests ("changesets") until they are merged. You describe it in a **bulk change spec** (YAML):
which repositories, what commands to run in each, and what the merge requests look like.
Sourcegull runs the steps, shows a preview of every diff, and on apply creates, updates or closes
the merge requests.

Bulk changes are on for every user by default; `"batchChanges.restrictToAdmins": true`
limits them to site admins and `"batchChanges.enabled": false` turns them off.

## Bulk change spec

```yaml
version: 2
name: update-go-1-23
description: Update the Go version in go.mod to 1.23.

# Repositories: every repository with a result for a search query, and/or explicit ones.
on:
  - repositoriesMatchingQuery: file:^go\.mod$ "go 1.22"
  - repository: gitlab.example.com/acme/tools
    branch: release        # default: the default branch; `branches:` for several

# Commands run in each repository, each in a container with the working copy as the
# working directory. The diff they leave behind becomes the merge request.
steps:
  - run: sed -i 's/^go 1\.22.*/go 1.23/' go.mod
    container: alpine:3
  - run: go mod tidy
    container: golang:1.23
    if: ${{ matches repository.name "*acme/*" }}

changesetTemplate:
  title: Update Go to 1.23
  body: |
    Updates `go.mod` (bulk change ${{ batch_change.name }}).
  branch: bulk/update-go-1-23
  commit:
    message: Update Go to 1.23
    author:
      name: Sourcegull
      email: sourcegull@example.com
  published: false         # true, false or "draft"; omit it to publish from the UI
```

`gull bulk new` writes a starter spec, and `gull bulk validate -f batch.yaml` checks one.
The editor in the UI validates as you type.

Spec fields:

| Field | Meaning |
|---|---|
| `version` | `2` (recommended) or `1`. |
| `name`, `description` | Name (letters, digits, `-`, `_`, `.`) and Markdown description. |
| `on` | `repositoriesMatchingQuery: <query>` and/or `repository: <name>` with optional `branch` or `branches`. |
| `workspaces` | Monorepo support: `rootAtLocationOf: <file>` makes each directory holding that file its own workspace and merge request; `in: <glob>` limits it to some repositories; `onlyFetchWorkspace` checks out only that directory. |
| `steps` | `run` (shell script), `container` (image), `env`, `files` (path → content to create in the container), `outputs` (named values for later steps and templates), `if` (skip the step unless true), `mount` (local files mounted into the container). |
| `transformChanges` | `group`: split one repository's diff into several merge requests by `directory`, each with its own `branch`. |
| `importChangesets` | Track existing merge requests: `repository` and `externalIDs` (merge request IIDs). |
| `changesetTemplate` | `title`, `body`, `branch`, `commit` (`message`, `author`), `fork` (push the branch to a fork), `published`. |

Text fields can contain template expressions `${{ … }}` in Go `text/template` syntax, for
example `${{ repository.name }}`, `${{ repository.branch }}`,
`${{ repository.search_result_paths }}`, `${{ previous_step.stdout }}`,
`${{ steps.modified_files }}`, `${{ outputs.<name> }}` and `${{ batch_change.name }}`, with the
functions `join`, `split`, `replace`, `join_if` and `matches`. A misspelled name is an error.

## Running a spec

### On the server (executors)

Create a bulk change under **Bulk changes → Create**, write the spec in the editor, and run
it. Sourcegull resolves the workspaces (repositories, branches, directories; repositories with a
`.batchignore` file are skipped), queues one job per workspace on the `batches` executor queue,
and shows each workspace's progress, logs and diff. Step results are cached, so re-running a
spec only runs what changed.

This needs at least one executor with the `batches` queue, with the Docker or the Kubernetes
runtime (Compose profile `executors`, or Helm `executor.enabled=true` with `queues: [batches]`); see
[architecture](architecture.md#executors). Secrets for steps (for example a registry token) are
**executor secrets**, set per user (User settings → Executor secrets) or for the instance
(Site admin → Executor secrets) and passed to steps that list them in `env`.

### Locally (`gull bulk`)

`gull bulk preview` and `gull bulk apply` run the steps on your machine's Docker and upload
the resulting diffs:

```sh
export SOURCEGULL_ENDPOINT=https://sourcegull.example.com SOURCEGULL_ACCESS_TOKEN=...
gull bulk preview -f batch.yaml             # prints a link to the preview
gull bulk apply -f batch.yaml -n acme       # apply in the organization `acme`
```

Useful flags: `-n` (namespace: user or organization; default yourself), `-j` (workspaces in
parallel; default the number of CPUs), `--timeout` (per workspace, default `60m`),
`--clear-cache`, `--skip-errors`, `--fail-fast`, `--keep-logs`, `--workspace bind|volume`,
`--cache` (default `~/.cache/sourcegull/batch`) and `--tmp`. `gull bulk <command> --help` lists all.
`gull batch` still works as an alias of `gull bulk`.

## Preview and apply

The preview lists every changeset the spec would create, update, close or leave alone, with
its diff. **Apply** makes it so: the reconciler pushes branches, opens or updates merge requests
and closes the ones the spec no longer produces. Re-applying an edited spec updates the existing
merge requests in place.

Publishing:

- `published: false` keeps changesets unpublished (nothing is pushed); `"draft"` opens draft
  merge requests; `true` opens them right away. `published` can also be a list of
  `repository glob: value` entries, where the last match wins.
- Without `published` in the spec, you publish changesets from the UI.
- `batchChanges.rolloutWindows` in the site configuration limits when and how fast merge
  requests are published, e.g. 10 per hour on weekdays.

## Credentials and commits

Sourcegull pushes branches and opens merge requests with a GitLab token with `api` scope:
your own (User settings → Bulk changes) or a site-wide one (Site admin → Bulk changes).
Changesets are published to GitLab only, so a credential for a GitHub or Gitea connection is
rejected as invalid input.

- **Commits signed by GitLab**: with `"batchChanges.commitViaAPI": true`, Sourcegull creates commits
  through GitLab's commit API instead of `git push`, so GitLab signs them where web commit
  signing is set up. Large changes (`commitViaAPIMaxBytes`, `commitViaAPIMaxFiles`), symlinks
  and submodules fall back to `git push`, unless `batchChanges.rejectUnverifiedCommit` makes
  them fail instead.
- **Forks**: `"batchChanges.enforceForks": true` (or `fork: true` in the changeset template)
  pushes branches to a fork in the credential user's namespace.
- `batchChanges.autoDeleteBranch` asks GitLab to delete the source branch on merge.

## Tracking

A bulk change's page shows every changeset with its state (unpublished, draft, open, merged,
closed), pipeline status, review state, diff and history, and a **burndown chart** of
changesets over time. Changesets can be filtered, searched and exported as CSV.

Bulk actions on selected changesets: comment, publish, merge, close, detach and retry.

Sourcegull keeps changesets current by syncing them from GitLab periodically and on merge request
and pipeline webhook events (add a project or group webhook pointing at the URL shown on the
code host connection page). Closing a bulk change can close its open merge requests.
`batchChanges.changesetsRetention` deletes changesets that no bulk change uses any more after
a while. Previews nobody applied are deleted in the background: a bulk change spec no bulk
change uses after 7 days, and an uploaded changeset spec never attached to a bulk change spec
after 2 days (the `expiresAt` the API shows).

Bulk change and changeset events can also be sent to other systems through
[outbound webhooks](features.md#outbound-webhooks).
