# AI agents (MCP)

Sourcegull runs a [Model Context Protocol](https://modelcontextprotocol.io) server at
`<externalURL>/.api/mcp`. Coding agents connected to it can search all your code, read files,
find symbols, definitions and references, and read history, with the permissions of the user
whose token they use. Output is compact text sized for a model's context: search results
grouped by file with line numbers, numbered file lines, one line per symbol, reference or
commit, with a hint when output was cut.

## Tools

All tools are read-only.

| Tool | Does |
|---|---|
| `search` | Search code with the [query language](search.md) (keyword by default; `repo:`, `file:`, `lang:`, `type:`, `case:`, `rev:`, `author:`, `after:`/`before:`, `select:`, boolean operators). Arguments: `query`, `repo`, `pattern_type`, `limit` (default 30), `max_lines_per_file` (default 10). |
| `read_file` | Read a file or a line range (`start_line`, `end_line`) at a revision, with line numbers. |
| `list_files` | List a directory, or with `recursive` every file below it; `pattern` filters paths. |
| `find_symbols` | Find definitions of functions, types, methods, constants, … by name (regexp or `exact`), optionally by `kind`, `language`, `repo` or extra `filters`. |
| `go_to_definition` | The definition of the identifier at a line of a file (precise data when available), with `context_lines` of the definition. A definition found at another version of the symbol's package says so, with both versions. |
| `find_references` | The usages of the identifier at a line of a file, across repositories (or `scope: repo`), paged. Each file's usages are tagged precise or search-based when an answer mixes them; a precise answer ends with a coverage line such as `Coverage: precise: 5 repositories · search-based: push-service (no index)`. Files whose usages were linked across package versions are tagged with the version their index has (`at version v1.0.54`), and the coverage line lists those repositories. |
| `find_implementations` | The implementations of the interface, trait or method at a line of a file, across repositories (or `scope: repo`), and for a concrete type or method the interfaces it implements; paged. Needs [precise data](code-navigation.md); without it the tool says so. |
| `list_repos` | The repositories the user can search, filtered by a name substring. |
| `list_branches` | Branches and/or tags of a repository, most recently committed first. |
| `commit_log` | Commit history of a revision, optionally of one path; one line per commit. |
| `get_commit` | A commit's message, author, changed files and diff (optionally limited to some paths). |
| `compare_revisions` | Commits and changed files between two revisions (`base...head`), optionally with the diff. |
| `blame` | Who last changed each line of a file, one line per hunk. |

Repositories are named as in Sourcegull (`gitlab.example.com/group/project`); a unique suffix such
as `group/project` also works. Revisions default to the default branch. The navigation tools
take `repo`, `path`, `line` and either the `symbol` on that line (preferred) or a `column`.

## Authentication

Clients authenticate in one of two ways:

- **OAuth sign-in** (clients that support MCP authorization, such as Claude Code, VS Code and
  Cursor): the client needs only the URL. The first time it connects, it opens the browser; the
  user signs in with any configured provider and allows the app on a consent page. Details
  [below](#oauth-sign-in).
- **Access tokens**: a [personal access token](permissions.md#access-tokens), sent as
  `Authorization: Bearer <token>` (the form MCP clients use) or `Authorization: token <token>`.
  Create a dedicated token per agent, so it can be revoked on its own.

Either way, each tool runs as the user, so repository permissions apply to every result; a
repository the user cannot read looks like one that does not exist.

The server speaks the Streamable HTTP transport, stateless and JSON-only: each `POST` carries one
JSON-RPC message (or a batch) and gets an `application/json` response; no session ID is issued.

## OAuth sign-in

Sourcegull is an OAuth 2.1 authorization server for its MCP endpoint, following the MCP
authorization specification:

1. A request without credentials gets `401` with
   `WWW-Authenticate: Bearer resource_metadata="<externalURL>/.well-known/oauth-protected-resource/.api/mcp"`.
2. The protected resource metadata (RFC 9728) names Sourcegull as the authorization server, whose
   metadata is at `/.well-known/oauth-authorization-server` (RFC 8414).
3. The client identifies itself with a client ID metadata document (an `https` URL as its
   `client_id`) or registers at `/.oauth/register` (Dynamic Client Registration, RFC 7591;
   rate limited per address). A metadata document is fetched only once the user has signed
   in (a browser that is not signed in just gets the sign-in page first), never from a private
   address or from a host in `webhooks.outbound.allowedHosts` (those are internal by intent),
   and is cached for an hour. When the fetch fails, the page shows a generic error; the reason
   is in the server log.
4. `/.oauth/authorize` takes the authorization code flow with PKCE (`S256` only) and the
   `resource` indicator (RFC 8707) of the MCP endpoint. The browser lands on the consent page,
   which shows the app's name and the host it redirects back to; the user signs in if needed
   and allows or denies. A registered app chose its own name, so when it redirects to a host
   that is not loopback the page warns that the code leaves the user's computer.
5. `/.oauth/token` exchanges the code for an access token (1 hour) and a refresh token (30 days).
   A code works once: presenting it again revokes the tokens issued for it (RFC 6749 §4.1.2).
   Refresh tokens rotate: each refresh returns a new one, and presenting a used one again
   revokes the whole grant. `/.oauth/revoke` (RFC 7009) revokes a grant from the client.

The worker's `oauth-janitor` deletes, every hour, authorization requests and codes a day after
they expire, registered apps that got no grant within 30 days, and metadata document apps
without a live grant a day after their document was last fetched.

Redirect URIs must be loopback (`http://127.0.0.1:<any port>/…`, `http://localhost:…`) or
`https`. Clients are public: they prove possession with PKCE, not a client secret.

Tokens are opaque (`sko_…` and `skr_…`), stored hashed, and work only at `/.api/mcp`: GraphQL and
the other APIs reject them. **User settings → Connected apps** lists the apps a user allowed,
with when they were last used, and revokes them; **Site admin → Connected apps** lists every
user's. Grants, refreshes, reuse of a rotated refresh token or a used code, and revocations are
[audit events](permissions.md#audit-events), and deactivating a user through SCIM revokes their
grants.

Site configuration:

| Key | Default | |
|---|---|---|
| `mcp.oauth.enabled` | `true` | Off: the OAuth endpoints answer 404, OAuth tokens stop working, and clients need access tokens. |
| `mcp.oauth.dynamicClientRegistration` | `true` | Off: only clients with a client ID metadata document can sign in. |
| `mcp.oauth.accessTokenTTL` | `"1h"` | Access token lifetime. |
| `mcp.oauth.refreshTokenTTL` | `"30d"` | Refresh token lifetime; a grant not refreshed within it ends. |
| `mcp.oauth.allowedRedirectHosts` | `[]` (any) | Hosts `https` redirect URIs may use, e.g. `["vscode.dev", "*.example.com"]`. Loopback redirect URIs are always allowed. |

Set `externalURL`: it is the issuer and the resource URL. Without it they are derived from the
request's `Host` header.

## Connecting an agent

**Connect an agent** in the sidebar (`/connect-agent`) has copy-paste setup for Claude Code,
Cursor, VS Code (Copilot), Windsurf, Codex CLI, Gemini CLI, Zed, JetBrains IDEs, Cline,
Continue and stdio-only clients. With OAuth sign-in on, it first asks how the agent signs in,
and shows only the steps for that choice:

- **Through the browser** (the default): the short form for clients that support MCP OAuth,
  for example

  ```sh
  claude mcp add --transport http sourcegull https://sourcegull.example.com/.api/mcp
  ```

- **With an access token**: the endpoint URL, a button that creates a token and fills it into
  every snippet, the per-agent setup and a `curl` test.

Without OAuth sign-in only the access token steps are shown. Both choices end with rules for
the agent's instructions file, example prompts and the tool list.

Examples with an access token in `SOURCEGULL_TOKEN`:

```sh
# Claude Code
claude mcp add --transport http --scope user sourcegull https://sourcegull.example.com/.api/mcp \
  --header "Authorization: Bearer $SOURCEGULL_TOKEN"

# Codex CLI
codex mcp add sourcegull --url https://sourcegull.example.com/.api/mcp --bearer-token-env-var SOURCEGULL_TOKEN

# Gemini CLI
gemini mcp add --transport http --scope user sourcegull https://sourcegull.example.com/.api/mcp \
  --header "Authorization: Bearer $SOURCEGULL_TOKEN"
```

Clients configured by JSON (Cursor `~/.cursor/mcp.json`, and similar for others):

```json
{
  "mcpServers": {
    "sourcegull": {
      "url": "https://sourcegull.example.com/.api/mcp",
      "headers": { "Authorization": "Bearer ${env:SOURCEGULL_TOKEN}" }
    }
  }
}
```

Clients that only start local (stdio) servers can use the `mcp-remote` bridge (needs Node.js):

```sh
npx -y mcp-remote https://sourcegull.example.com/.api/mcp --header "Authorization: Bearer ${SOURCEGULL_TOKEN}"
```

## Checking the endpoint

```sh
curl -sS https://sourcegull.example.com/.api/mcp \
  -H "Authorization: Bearer $SOURCEGULL_TOKEN" \
  -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}'
```

A `401` means the token is missing, wrong or (for OAuth tokens) expired.

## Example prompts

- "Use sourcegull to find where `RetryPolicy` is defined and list every service that constructs one."
- "Search all repositories for calls to the deprecated `v1/users` endpoint and summarize them by
  team."
- "Read the last 20 commits touching `billing/` in `acme/api` and explain what changed."
