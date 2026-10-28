# Other features

## Markdown files and READMEs

The file view shows `.md` and `.markdown` files rendered, with a **Preview** / **Code**
toggle. Preview is the default; **Code** keeps code navigation, blame and line links. A link
to lines (`#L10`) opens the code, and `?view=code` or `?view=preview` in the URL picks a view.
A directory page shows its README (`README.md`, then `README.markdown`, then `README`, in any
case) below the listing.

Rendering covers headings (with `#anchor` links), nested and task lists, tables, code,
quotes, links and images. HTML in the Markdown is not rendered: tags are dropped and their
text kept. Relative links open files and directories in the same repository at the same
revision, and relative images load through the raw file endpoint, so the viewer's repository
permissions apply. Images from other hosts are shown as links, since the content security
policy allows images from Sourcegull only. SVG images are shown as links to the file.

## Repository metadata

Repositories carry key-value pairs and tags (a key with no value), such as `team:search` or
`deprecated`, set on the repository's **Settings** tab (`/<repo>/-/settings`) or with the
GraphQL mutations `addRepoKeyValuePair`, `updateRepoKeyValuePair` and `deleteRepoKeyValuePair`
(a null value makes a tag). Changing them needs the `REPO_METADATA#WRITE` permission
([roles](permissions.md#roles-rbac)) and read access to the repository; the tab is shown only
to users with the permission. Everyone who can read the repository sees its metadata
(`Repository.keyValuePairs`). Searches select repositories by it with
[`repo:has.meta()` and `repo:has.tag()`](search.md); changes apply to searches within seconds.

## Trends

Trends (`/trends`) chart how code changes over time: for example, how many files
still use a deprecated API, per repository, week by week.

Trend types, each with a creation wizard and a live preview:

- **Search**: one or more series, each a search query whose match count is recorded at
  regular points (hourly to yearly).
- **Capture group**: a regexp with a capture group; each distinct captured value (for example a
  version number) becomes its own series.
- **Group by**: the current result count of a query grouped by repository, language, path,
  author or commit date, as a bar chart.
- **Language usage**: lines of code per language in a repository, as a pie chart.

A trend covers a list of repositories, the repositories matching a query, or all of them.
History is **backfilled** from git: each repository counts the oldest point in full and every
later point from the previous one through a tree diff, so the cost follows the repository's
size and churn rather than the number of points. New points are recorded as commits arrive.
Site admins can list the backfill queue through the GraphQL API
(`insightAdminBackfillQueue`) and retry jobs or move them to the front or back of the queue. Backfill speed is set by `insights.query.worker.rateLimit` and
`insights.query.worker.concurrency`.

Trends live on **dashboards** that are private, shared with an organization, or global.
Filters (repositories, search context) on a trend can be saved as its defaults or as a new
view. Clicking a point opens the commits behind it (`type:diff`), and the data exports as CSV.
Each viewer sees only the counts of repositories they can read.

## Watches

A watch (`/watches`) follows a `type:commit` or `type:diff` query and notifies
you when new commits match, for example `type:diff repo:acme/ -file:test select:commit.diff.added AWS_SECRET`.

- Watches are evaluated on the commits each fetch adds, with the owner's permissions, so new
  matches are reported shortly after a push and old history is not searched again.
- Actions: **email** (with an optional extra header such as `Approved:` for moderated lists),
  a **webhook** (JSON payload), a **Slack-compatible webhook**, a **Mattermost** incoming
  webhook (Markdown) and a **Telegram** bot message to a chat (a numeric chat ID or a public
  channel's `@username`; add the bot to the chat first). Webhook URLs, bot tokens and email
  headers are encrypted at rest and write-only: the API and the edit form show only the host
  (for a header, the field name) and the last characters, and leaving the field empty keeps
  (and "Send test" uses) the stored value; **Remove the stored header** clears a header. Each
  action can include the matching results.
- A watch belongs to a user or an organization, and keeps a log of its runs and deliveries. An
  organization's watch runs with the permissions of the member who last saved, enabled or
  disabled it; the watch's page names that member. Deactivated users get no watch emails.
- An email to an organization gives each member only the results in repositories they can read,
  and none when none are left; the email says that results are limited this way. Webhook,
  Slack, Mattermost and Telegram messages carry the results of the member the watch runs as.
  The watch's log shows each viewer only the repositories and result counts they can read.
- **Unfiltered results.** A site admin can make a watch send every email recipient all of its
  results, found with the admin's access, after a red warning; `watches.allowUnfilteredResults`
  lets every user do it, after a warning that they are responsible for sharing code that
  recipients may not be allowed to read. The watch shows who turned it on and when, and the
  emails carry a warning. It is turned off when someone else saves, enables or disables the
  watch, and is ignored while the member it runs as may no longer turn it on.

Email needs `email.address` and `email.smtp` in the site configuration; a failed email is
retried up to five times, backing off from 30 seconds. Webhook, Slack, Mattermost and Telegram
actions are delivered as [outbound webhooks](#outbound-webhooks): each notification is
rendered when the watch finds results and queued on the outbound webhook queue (the
`outbound_webhook_jobs` background job queue), and the outbound webhook workers send it through
the same address checks (`webhooks.outbound.allowedHosts`) with the same retries (five
attempts over about 7.5 minutes). Every attempt is recorded in the outbound webhook delivery
log (encrypted, with the URL reduced to its origin, kept for `webhooks.outbound.logRetention`);
the watch's log shows each notification's outcome and last error. A notification still waiting
for an attempt is canceled when its watch or action is turned off. A webhook action's request
carries the `X-Sourcegull-Webhook-Delivery` header, the same on every retry of one notification.
Without [encryption keys](configuration.md#encryption-keys), these actions are queued and
retried with email instead.

## Notebooks

Notebooks (`/notebooks`) are documents made of blocks: **Markdown**, **search query** (runs
the query and shows the results), **file** (a range of lines of a file at a revision) and
**symbol** (a symbol's definition). Use them for onboarding guides, incident write-ups or
code walkthroughs.

A notebook belongs to a user or an organization and is private or public; it can be starred
and found by a full-text search over its title and content. Blocks that refer to repositories a
viewer cannot read are hidden from that viewer, and kept when they save. Notebooks export to
and import from Markdown files (`.snb.md`).

## Code ownership

Sourcegull answers "who owns this file?" from three sources, shown in the file view's ownership
panel with the reason for each owner:

- **CODEOWNERS** files in GitLab syntax (sections, optional sections, default owners, users,
  groups and emails), found where the repository's code host looks for them, the first one
  found in this order:
  - GitLab: `CODEOWNERS`, `docs/CODEOWNERS`, `.gitlab/CODEOWNERS`
  - GitHub: `.github/CODEOWNERS`, `CODEOWNERS`, `docs/CODEOWNERS`
  - Gitea: `CODEOWNERS`, `docs/CODEOWNERS`, `.gitea/CODEOWNERS`

  Patterns are read as GitLab globs on every host, so a Gitea file with regular expression
  patterns does not match as it does on Gitea. Site admins can also upload a CODEOWNERS file
  for a repository (Site admin → CODEOWNERS files).
- **Assigned owners**: users and teams assigned to a file, directory or repository in the UI.
- **Signals**: recent contributors and recent viewers of a file (Site admin → Ownership
  signals turns them on and excludes repositories).

In search, `file:has.owner(@alice)` finds files an owner owns, `file:has.owner()` finds files
with any owner, and `select:file.owners` lists the owners of the matching files. Site admin →
Ownership analytics shows how much of the code has an owner.

Teams can be created in Sourcegull or synced hourly from GitLab groups with
`"own.syncTeamsFromGitLabGroups": true`.

## Compute

Compute (Code → Compute, or **Compute** on the search results page to open the current query)
runs a search and transforms its results. A compute query is a search query with one pattern,
always a regular expression:

- a plain pattern lists each match with its capture groups;
- `content:output(<regexp> -> <template>)` writes the template once per match, joined by
  `separator:` (a newline by default); `content:output.extra(…)` also returns the repository;
- `content:replace(<regexp> -> <template>)` rewrites each matching file and shows the change
  as a diff against the original;
- add `.structural` (`content:output.structural(…)`) for a structural pattern with `:[holes]`.

Templates use capture groups (`$1`, `$name`), holes (`:[x]`) and metadata: `$repo`, `$path`,
`$lang`, `$commit`, `$content`, and on commit and diff results `$author`, `$email` and `$date`
(`$date.year`, `$date.month`, `$date.month.name`, `$date.day`). `type:commit`, `type:diff`,
`type:path`, `type:repo` and `select:` change what the pattern runs against.

| Query | Gives |
| --- | --- |
| `repo:acme/app (\w+)Error` | every `…Error` name, with `FooError` → `$1` = `Foo` |
| `lang:rust content:output(fn (\w+)\( -> $repo:$path:$1)` | one `repo:path:function` line per function |
| `content:replace(fmt.Println -> log.Println) lang:go` | a diff of each Go file with the call replaced |
| `type:commit content:output(#(\d+) -> $1 $author $date.year)` | issue number, author and year of each commit message that references an issue |

The output can be copied or downloaded as a text file, and each result links to its file.
Results stream from `/.api/compute/stream` (GraphQL: `compute(query:)`); a query runs for at
most 60 seconds with the viewer's repository permissions. Capture-group trends use the same
engine.

## Usage analytics

Site admin → Analytics shows how the instance is used: daily, weekly and monthly active users,
searches by type and latency, code navigation by repository and language, and the use of
notebooks, watches, bulk changes and trends. Site admin → User activity lists each user's
activity.

The web app logs page views and clicks into these event logs: search result clicks, links to
the code host, code copied from a file or a search result (the file page's Copy button or a
text selection), notebook block runs, and hovers and data point clicks on trend charts.

The numbers come from this instance's own event logs, aggregated by a background job every 30
minutes. Nothing is sent outside the instance. Raw events are kept for `eventLogs.retention`
and the daily rollups for `eventLogs.rollupRetention`.

## Outbound webhooks

Site admins can register webhooks (Site admin → Outbound webhooks) that receive JSON events
when bulk changes and changesets change:

`batch_change:apply`, `batch_change:close`, `batch_change:delete`, `changeset:publish`,
`changeset:publish_error`, `changeset:update`, `changeset:update_error`, `changeset:close`.

Each delivery is signed with the webhook's secret (HMAC) and retried on failure; every attempt
is logged on the webhook's page. The URL is write-only like the secret, since it may carry a
token: the API, the edit form and the delivery log show only its origin and last characters,
and saving with the URL field empty keeps the stored URL. Deliveries to private, loopback, link-local and other
non-public addresses are refused unless the host is listed in
`webhooks.outbound.allowedHosts`. Logs are kept for `webhooks.outbound.logRetention`.
The same page lists every delivery attempt of watch actions (webhook, Slack, Mattermost,
Telegram) under **Watch deliveries**, with the watch, channel, status and error, and only the
destination's host (none for Telegram), since the URL may hold a token; **Only failed** narrows it.

## Site administration

**Site admin** (for site admins) groups the operational pages:

| Section | Pages |
|---|---|
| Overview | status summary, [analytics](#usage-analytics), user activity |
| Repositories | code host connections (settings form with token setup help, JSON editor, connection test before saving, sync now, sync history, webhook URL); repositories (clone, fetch and index status per branch, errors, re-clone, re-index, fetch now); repo nodes (disk usage, [rebalancing](architecture.md#repo-nodes-and-rebalancing)); repository permissions |
| Users & access | users, organizations, teams, roles, access requests ([permissions](permissions.md)) |
| Configuration | site configuration, global settings, feature flags (with per-user and per-organization overrides), outbound webhooks |
| Code intelligence | code graph policies, inference script, executors, executor secrets ([code navigation](code-navigation.md)) |
| Bulk changes | site-wide GitLab credentials |
| Code ownership | CODEOWNERS files, ownership signals, ownership analytics |
| Monitoring | background jobs (every scheduled routine and job queue, with retry and cancel), slow requests and searches, outbound requests to code hosts (secrets redacted), incoming webhook logs |
| Maintenance | updates (upgrade readiness, schema drift), out-of-band migrations |

A status icon in the top bar shows site admins what needs attention: repositories cloning or
failing, a repo node low on disk, or configuration problems.

Metrics, dashboards and alerts are described in [observability](observability/).
