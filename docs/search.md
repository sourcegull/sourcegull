# Search

Sourcegull searches file contents, paths, symbols, repositories, commits and diffs across every
repository you can read. Results stream in as they are found, with filter suggestions (facets)
in the sidebar and an aggregation view (counts by repository, path, author, …).

The default branch of each repository is indexed, plus any branches listed in the site
configuration's `search.index.branches`. Other revisions (branches, tags, commits) are searched
directly from git, a bit more slowly, with the same results.

Binary files and files over 1 MiB are found by path but not by content. To search generated or
other large files by content, list their paths in `search.index.alwaysIndex` (files up to
16 MiB):

```jsonc
"search.index.alwaysIndex": {
  "*": ["**/schema.graphql"],              // every repository
  "gitlab.com/acme/api": ["gen/**/*.ts"]
}
```

Patterns match the whole path from the repository root: `*` and `?` stay within a directory,
`**` spans any number of them, `{a,b}` and `[abc]` work as in a shell.

## Patterns

A query is a mix of **patterns** (text to find) and **filters** (`field:value`). How patterns
are read depends on the pattern type, which you choose with the toggle in the search box,
with `patterntype:` in the query, or by default with the `search.defaultPatternType`
[setting](configuration.md#settings-and-the-settings-cascade).

| Type | Behaviour | Example |
|---|---|---|
| `keyword` (default) | Every term must occur in the file, in any order. `"quoted phrases"` (or `'...'`) match exactly, `/.../` is a regular expression. Ranked with BM25. | `http client timeout`, `"new Server(" lang:java`, `/fn \w+_test/` |
| `standard` | The pattern is literal text, spaces included; `/.../` is a regular expression. | `foo(bar, baz)` |
| `literal` | The pattern is literal text, spaces included. | `a || b` |
| `regexp` | The pattern is a regular expression; `"..."` is a literal. | `func\s+New\w+\(` |
| `structural` | The pattern is a code template with holes that match balanced code. | `fmt.Errorf(:[msg], err)` |

Whitespace inside a phrase matches as written: `foo  bar` (standard or literal) and
`"foo  bar"` (keyword) find two spaces, not one, and a tab finds a tab. Empty delimiters are
text: `""`, `''` and `//` find those two characters.

Matching is case-insensitive unless the query has `case:yes`.

### Structural patterns

A structural template matches source text whose brackets balance the same way, ignoring
differences in whitespace and comments:

| Hole | Matches |
|---|---|
| `:[x]` or `...` | anything, including nothing and line breaks, with balanced `()`, `[]`, `{}` |
| `:[[x]]` | an identifier (letters, digits, `_`) |
| `:[x.]` | a run of non-space characters other than brackets |
| `:[x:e]` | an expression: characters and balanced groups up to whitespace, e.g. `a.b(c, d)[0]` |
| `:[x\n]` | the rest of the line |
| `:[ x]` | spaces and tabs |
| `:[x~regex]` | what `regex` matches |

Holes with the same name must match the same text. `rule:` adds conditions, e.g.
`rule:'where :[a] == :[b]'` or `where :[x] != "nil"`. Structural search runs on indexed
repositories and does not combine with `type:` or `case:`.

## Filters

Filters narrow the search. Most filters can be negated with `-` (`-file:_test\.go$`) or `NOT`.

### Repositories

| Filter | Meaning |
|---|---|
| `repo:<regexp>` (`r:`) | Repositories whose name matches. `repo:^gitlab\.example\.com/acme/api$` for one repository. |
| `repo:<regexp>@<rev>[:<rev>...]` | Search these revisions (branches, tags or commits) of the matched repositories. |
| `rev:<rev>` (`revision:`) | Revisions to search, for every matched repository. |
| `rev:at.time(<time>[, <ref>])` | The commit that was at the tip of a branch at a point in time, e.g. `rev:at.time(2026-01-01)`. |
| `context:<name>` | A [search context](#search-contexts); `global` (all repositories) by default. |
| `fork:yes\|no\|only` | Include forks (excluded by default, unless the `search.includeForks` setting is on). |
| `archived:yes\|no\|only` | Include archived repositories (excluded by default; setting `search.includeArchived`). |
| `visibility:any\|public\|private` | Repository visibility. |
| `index:yes\|no\|only` | Use indexed and unindexed revisions (default), only unindexed ones, or only the index. |

Repository predicates (the value is a function call):

| Predicate | Repositories that … |
|---|---|
| `repo:has.file(path:<regexp> content:<regexp>)` | contain a file with this path and/or content |
| `repo:has.path(<regexp>)` | contain a matching path |
| `repo:has.content(<regexp>)` | contain matching content |
| `repo:has.commit.after(<time>)` | have a commit after the time, e.g. `1 month ago` |
| `repo:has.description(<regexp>)` | have a matching description on the code host |
| `repo:has.topic(<topic>)` | have a topic on the code host |
| `repo:has.meta(<key>:<value>)` | have this [metadata](features.md#repository-metadata) pair; `repo:has.meta(<key>)` matches any value and `repo:has.meta(<key>:)` a key with no value. Keys and values match exactly (case-sensitive), or as a regexp written `/…/`, e.g. `repo:has.meta(/^team/:/^search/)`. |
| `repo:has.tag(<tag>)` | have this metadata tag (a key with no value) |

`contains.file`, `contains.path`, `contains.content` and `contains.commit.after` are aliases,
as are `repo:has.key(<key>)` for `repo:has.meta(<key>)` and `repo:has(<key>:<value>)` for
`repo:has.meta(<key>:<value>)`. Every repository predicate can be negated (`-repo:has.tag(legacy)`).

### Files and content

| Filter | Meaning |
|---|---|
| `file:<regexp>` (`f:`, `path:`) | Files whose path matches. |
| `lang:<language>` (`l:`, `language:`) | Files in a language, e.g. `lang:go`, `lang:typescript`. |
| `content:"<text>"` | Search contents for this literal text, whatever the pattern type; files must also match the pattern, if any (`-content:` excludes files with the text). Useful for text that looks like a filter. |
| `file:has.content(<regexp>)` | Files that contain a match, while the pattern matches something else. |
| `file:has.owner([<owner>])` | Files owned by a user or team ([code ownership](features.md#code-ownership)); without an argument, files with any owner. |
| `file:has.contributor(<regexp>)` | Files with a commit by a matching author. |
| `case:yes\|no` | Case-sensitive matching. |

### Result type and output

| Filter | Meaning |
|---|---|
| `type:file` | File contents (the default, together with paths and repository names). |
| `type:path` | File paths only. |
| `type:symbol` | Symbol definitions (functions, types, methods, …) whose name matches. |
| `type:repo` | Repositories whose name matches. |
| `type:commit` | Commit messages. |
| `type:diff` | Lines added or removed by commits. |
| `select:<type>` | Show what the results contain instead of the results: `repo`, `file`, `file.directory`, `file.path`, `file.owners`, `content`, `symbol`, `symbol.<kind>` (e.g. `symbol.function`, `symbol.class`), `commit`, `commit.diff.added`, `commit.diff.removed`. |
| `count:<n>\|all` | Stop after this many results (default 500; site configuration `search.limits.streamingMaxResults`). |
| `timeout:<duration>` | Search time limit, e.g. `30s`, `1m30s` (default 20 s, at most 60 s; site configuration `search.limits`). At the limit, or when the search is closed, the repo nodes stop their part of it too (content, commit and diff searches, and `repo:has.file()` checks). |

### Commit and diff filters

These need `type:commit` or `type:diff`:

| Filter | Meaning |
|---|---|
| `author:<regexp>`, `committer:<regexp>` | Name or email of the author or committer. |
| `message:<regexp>` (`m:`, `msg:`) | Commit message. |
| `before:<time>` (`until:`), `after:<time>` (`since:`) | Commit date: `2026-01-31`, `"3 weeks ago"`, `yesterday`. |

Example: `type:diff repo:acme/api after:"1 week ago" select:commit.diff.added password`.

## Boolean operators and grouping

- Terms next to each other are combined: in keyword queries every term must match the file; in
  standard and literal queries, adjacent words are one pattern with the space included.
- `AND`, `OR` and `NOT` (any case) combine patterns and filters; `AND` binds tighter than `OR`.
  Parentheses group: `(foo OR bar) lang:go`, `repo:a OR (repo:b file:\.md$)`.
- `NOT foo` or `-file:x` excludes.
- In standard and literal queries, balanced parentheses without filters or operators are part
  of the pattern (`copyright (c) 2026`). A `(` that is never closed is read as text.

## Quoting

- Filter values containing spaces need quotes: `file:"my docs/"`, `after:"2 weeks ago"`.
- In keyword and regexp queries, `"..."` and `'...'` are exact literals; escape a quote inside
  with `\"`.
- `/.../` is a regular expression in keyword and standard queries. Inside it, `\/` is a slash
  and every other escape is the regular expression's own: `/C:\\Users/` matches `C:\Users`.
- To search for text that looks like a filter (`foo:bar`), quote it or use `content:"foo:bar"`.
  Unknown fields are treated as text.

Invalid queries (an unknown value, a repeated `count:`, a negated filter that cannot be negated,
`author:` without `type:commit`) are reported in the search box before the search runs.

## Ranking

Results are ordered by score within one search. A file scores by its best match: a match on a
symbol definition counts most, then a whole-word match, then a partial match, with extra weight
when every term matches and for matches in the file name. Keyword queries score with BM25 over
their terms instead. Test, vendored and generated files are scored down. Ties are broken by the
file's [static rank from the code graph](code-navigation.md#ranking), shallower paths and the
repository's rank.

## Search contexts

A search context is a named set of repositories to search, selected with `context:` or the
dropdown in the search box. Create and manage them under **Search contexts** (`/contexts`):

- **Instance contexts** (`context:backend`) are created by site admins; **user and
  organization contexts** are written `context:@alice/frontend` or `context:@acme/infra`.
- A context either **lists repositories** (each with the revisions to search), or is
  **defined by a query** of repository filters, e.g. `repo:^gitlab\.example\.com/acme/ fork:no`.
- A context is private or public (public instance contexts are created by site admins). You
  can star contexts and choose one as your default.

## Saved searches

Save a query from the results page, or under **Saved searches** (`/saved-searches`). A saved
search belongs to you or to an organization; it is either secret (visible to you, or to the
organization's members) or public (visible to every user). To be notified
of new results, create a [watch](features.md#watches) from the query.

## Search jobs

A search job (`/search-jobs`) runs a query **exhaustively** in the background, over every
repository and revision it names, without the result limit of interactive search: up to a
million results and one hour per repository revision (`search.limits.exhaustiveMaxResults`
and `exhaustiveTimeoutSeconds`). Use it for audits and exports.

- Create one from the results page (**Create search job**) or the search jobs page.
- Supported queries: one query without `OR` between filter groups, without `select:`, file
  predicates, structural patterns or a lone `/.*/`, returning file, path, commit or diff
  results.
- The job runs with your permissions. Its progress, per-repository status and log are shown on
  the page; it can be cancelled or deleted.
- Results download as JSONL or CSV (`/.api/search/export/<id>.jsonl`, `.csv`, and `.log` for the
  log). Only the creator and site admins see a job. Old jobs are removed after
  `searchJobs.retention`.

## APIs

- `GET /.api/search/stream?q=<query>` streams results as server-sent events (matches,
  progress, filters, alerts).
- GraphQL `search`, `searchQueryAggregate` (aggregations) and the saved search, search context
  and search job types at `/.api/graphql`.
- The [MCP server](mcp.md) exposes search to AI agents.
