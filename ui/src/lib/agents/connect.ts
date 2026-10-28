/**
 * "Connect an agent": copy-paste setup for AI coding agents that talk
 * to Sourcegull's MCP server at `/.api/mcp`. Each snippet is built from the endpoint URL and the
 * access token (or a placeholder), with JSON/TOML strings escaped properly.
 */

/** The MCP endpoint path (crates/mcp `PATH`). */
export const MCP_PATH = '/.api/mcp';

/** Placeholder shown while no token has been created on the page. */
export const TOKEN_PLACEHOLDER = '<TOKEN>';

/** Environment variable the snippets read the token from, where the client supports it. */
export const TOKEN_ENV = 'SOURCEGULL_TOKEN';

/** Name the server is registered under in every client. */
export const SERVER_NAME = 'sourcegull';

/** The endpoint URL for an instance origin such as `https://sourcegull.example.com`. */
export function mcpEndpoint(origin: string): string {
	return origin.replace(/\/+$/, '') + MCP_PATH;
}

export interface Snippet {
	title: string;
	/** Where the snippet goes: a file path, or "Terminal". */
	where: string;
	code: string;
	/** Short note under the snippet. */
	note?: string;
}

export interface AgentGuide {
	id: string;
	label: string;
	/** The client's own MCP documentation. */
	docs: string;
	intro?: string;
	snippets: Snippet[];
}

const json = (v: unknown) => JSON.stringify(v, null, 2);
/** A TOML basic string; JSON string escaping is valid TOML for these values. */
const toml = (s: string) => JSON.stringify(s);
/** Single-quotes `s` for a POSIX shell. */
export const shellQuote = (s: string) => `'${s.replace(/'/g, `'\\''`)}'`;

/** The `export SOURCEGULL_TOKEN=…` line for clients that read the token from the environment. */
export function exportLine(token: string): string {
	return `export ${TOKEN_ENV}=${shellQuote(token)}`;
}

/** The per-agent setup guides for endpoint `url` and access token `token` (or the placeholder). */
export function agentGuides(url: string, token: string = TOKEN_PLACEHOLDER): AgentGuide[] {
	const bearer = `Bearer ${token}`;
	const envNote = `Set ${TOKEN_ENV} in the environment the app starts from (for example your shell profile): ${exportLine(token)}`;
	const remote = (env: Record<string, string>, args: string[] = []) => ({
		command: 'npx',
		args: ['-y', 'mcp-remote', url, '--header', 'Authorization:${AUTH_HEADER}', ...args],
		env
	});

	return [
		{
			id: 'claude-code',
			label: 'Claude Code',
			docs: 'https://code.claude.com/docs/en/mcp',
			snippets: [
				{
					title: 'Add for yourself (all projects)',
					where: 'Terminal',
					code: `claude mcp add --transport http --scope user ${SERVER_NAME} ${url} \\\n  --header ${shellQuote(`Authorization: ${bearer}`)}`,
					note: 'Use --scope local (the default) for the current project only. Check with `claude mcp list`, or /mcp inside Claude Code.'
				},
				{
					title: 'Share with a project',
					where: '.mcp.json (project root, commit it)',
					code: json({
						mcpServers: {
							[SERVER_NAME]: { type: 'http', url, headers: { Authorization: `Bearer \${${TOKEN_ENV}}` } }
						}
					}),
					note: `Each developer sets their own token: ${exportLine(token)}`
				}
			]
		},
		{
			id: 'cursor',
			label: 'Cursor',
			docs: 'https://cursor.com/docs/context/mcp',
			snippets: [
				{
					title: 'Configuration',
					where: '~/.cursor/mcp.json (all projects) or .cursor/mcp.json (one project)',
					code: json({
						mcpServers: { [SERVER_NAME]: { url, headers: { Authorization: `Bearer \${env:${TOKEN_ENV}}` } } }
					}),
					note: `${envNote}. Or put the token in place of \${env:${TOKEN_ENV}} in a file you do not commit.`
				}
			]
		},
		{
			id: 'vscode',
			label: 'VS Code / Copilot',
			docs: 'https://code.visualstudio.com/docs/copilot/reference/mcp-configuration',
			intro: 'For GitHub Copilot agent mode. VS Code asks for the token the first time the server starts and stores it securely, so the file can be committed.',
			snippets: [
				{
					title: 'Workspace configuration',
					where: '.vscode/mcp.json',
					code: json({
						inputs: [{ type: 'promptString', id: 'sourcegull-token', description: 'Sourcegull access token', password: true }],
						servers: {
							[SERVER_NAME]: { type: 'http', url, headers: { Authorization: 'Bearer ${input:sourcegull-token}' } }
						}
					}),
					note: 'For all workspaces, run "MCP: Open User Configuration" from the Command Palette and add the same entries.'
				}
			]
		},
		{
			id: 'windsurf',
			label: 'Windsurf',
			docs: 'https://docs.windsurf.com/windsurf/cascade/mcp',
			snippets: [
				{
					title: 'Configuration',
					where: '~/.codeium/windsurf/mcp_config.json (newer builds: Settings → Cascade → MCP servers → View raw config)',
					code: json({
						mcpServers: {
							[SERVER_NAME]: { serverUrl: url, headers: { Authorization: `Bearer \${env:${TOKEN_ENV}}` } }
						}
					}),
					note: envNote
				}
			]
		},
		{
			id: 'codex',
			label: 'Codex CLI',
			docs: 'https://developers.openai.com/codex/mcp',
			intro: 'Codex sends the token from an environment variable as a bearer token.',
			snippets: [
				{ title: 'Set the token', where: 'Shell profile', code: exportLine(token) },
				{
					title: 'Add the server',
					where: 'Terminal',
					code: `codex mcp add ${SERVER_NAME} --url ${url} --bearer-token-env-var ${TOKEN_ENV}`
				},
				{
					title: 'Or edit the configuration',
					where: '~/.codex/config.toml',
					code: `[mcp_servers.${SERVER_NAME}]\nurl = ${toml(url)}\nbearer_token_env_var = ${toml(TOKEN_ENV)}`
				}
			]
		},
		{
			id: 'gemini',
			label: 'Gemini CLI',
			docs: 'https://geminicli.com/docs/tools/mcp-server/',
			snippets: [
				{
					title: 'Add the server',
					where: 'Terminal',
					code: `gemini mcp add --transport http --scope user ${SERVER_NAME} ${url} \\\n  --header ${shellQuote(`Authorization: ${bearer}`)}`
				},
				{
					title: 'Or edit the settings',
					where: '~/.gemini/settings.json (or .gemini/settings.json in a project)',
					code: json({ mcpServers: { [SERVER_NAME]: { httpUrl: url, headers: { Authorization: bearer } } } })
				}
			]
		},
		{
			id: 'zed',
			label: 'Zed',
			docs: 'https://zed.dev/docs/ai/mcp',
			snippets: [
				{
					title: 'Settings',
					where: '~/.config/zed/settings.json (Zed → Settings → Open Settings)',
					code: json({ context_servers: { [SERVER_NAME]: { url, headers: { Authorization: bearer } } } }),
					note: 'Or use Settings → AI → MCP Servers → Add Server → Add Remote Server.'
				}
			]
		},
		{
			id: 'jetbrains',
			label: 'JetBrains',
			docs: 'https://www.jetbrains.com/help/ai-assistant/mcp.html',
			intro: 'AI Assistant and Junie run local (stdio) MCP servers reliably, so these use the mcp-remote bridge, which needs Node.js.',
			snippets: [
				{
					title: 'AI Assistant',
					where: 'Settings → Tools → AI Assistant → Model Context Protocol (MCP) → Add → As JSON',
					code: json({ mcpServers: { [SERVER_NAME]: remote({ AUTH_HEADER: bearer }) } })
				},
				{
					title: 'Junie',
					where: '~/.junie/mcp/mcp.json (or .junie/mcp/mcp.json in a project)',
					code: json({ mcpServers: { [SERVER_NAME]: remote({ AUTH_HEADER: bearer }) } })
				}
			]
		},
		{
			id: 'cline',
			label: 'Cline',
			docs: 'https://docs.cline.bot/mcp/configuring-mcp-servers',
			snippets: [
				{
					title: 'Configuration',
					where: 'Cline panel → MCP Servers → Configure → Configure MCP Servers (cline_mcp_settings.json)',
					code: json({
						mcpServers: {
							[SERVER_NAME]: { type: 'streamableHttp', url, headers: { Authorization: bearer }, disabled: false }
						}
					}),
					note: 'Keep "type": "streamableHttp"; without it Cline uses the legacy SSE transport.'
				}
			]
		},
		{
			id: 'continue',
			label: 'Continue',
			docs: 'https://docs.continue.dev/customize/deep-dives/mcp',
			snippets: [
				{
					title: 'Server file',
					where: '.continue/mcpServers/sourcegull.yaml (workspace root)',
					code: [
						'name: Sourcegull',
						'version: 0.0.1',
						'schema: v1',
						'mcpServers:',
						'  - name: Sourcegull',
						'    type: streamable-http',
						`    url: ${toml(url)}`,
						'    requestOptions:',
						'      headers:',
						`        Authorization: "Bearer \${{ secrets.${TOKEN_ENV} }}"`
					].join('\n'),
					note: `Store the token as a local secret: add ${TOKEN_ENV}=${token} to ~/.continue/.env.`
				}
			]
		},
		{
			id: 'other',
			label: 'Other (stdio)',
			docs: 'https://github.com/geelen/mcp-remote',
			intro: 'For clients that only start local (stdio) servers. mcp-remote bridges stdio to Sourcegull over HTTP; it needs Node.js.',
			snippets: [
				{
					title: 'Command',
					where: 'Terminal',
					code: `${exportLine(token)}\nnpx -y mcp-remote ${url} --header "Authorization: Bearer \${${TOKEN_ENV}}"`
				},
				{
					title: 'Typical client configuration',
					where: "The client's MCP settings file",
					code: json({ mcpServers: { [SERVER_NAME]: remote({ AUTH_HEADER: bearer }) } }),
					note: 'Some clients split arguments on spaces, so the header has no space after the colon and the value comes from AUTH_HEADER.'
				}
			]
		}
	];
}

/** Where an instance announces its OAuth authorization server (404 while it is off). */
export const OAUTH_METADATA_PATH = '/.well-known/oauth-authorization-server';

/**
 * Setup for agents that sign in through OAuth: the URL alone, then a browser sign-in the
 * first time the agent connects.
 */
export function oauthSnippets(url: string): Snippet[] {
	return [
		{
			title: 'Claude Code',
			where: 'Terminal',
			code: `claude mcp add --transport http ${SERVER_NAME} ${url}`,
			note: 'Then run /mcp inside Claude Code and choose Authenticate.'
		},
		{
			title: 'VS Code / Copilot',
			where: '.vscode/mcp.json',
			code: json({ servers: { [SERVER_NAME]: { type: 'http', url } } })
		},
		{
			title: 'Cursor',
			where: '~/.cursor/mcp.json',
			code: json({ mcpServers: { [SERVER_NAME]: { url } } })
		}
	];
}

/** Whether the instance at the page's origin offers OAuth sign-in to MCP clients. */
export async function oauthAvailable(doFetch: typeof fetch = fetch): Promise<boolean> {
	try {
		const res = await doFetch(OAUTH_METADATA_PATH, { headers: { Accept: 'application/json' } });
		return res.ok;
	} catch {
		return false;
	}
}

/** curl commands that check the endpoint and the token. */
export function testCommands(url: string, token: string = TOKEN_PLACEHOLDER): Snippet[] {
	const head = `curl -sS ${url} \\\n  -H "Authorization: Bearer $${TOKEN_ENV}" \\\n  -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' \\\n  -d `;
	const init = {
		jsonrpc: '2.0',
		id: 1,
		method: 'initialize',
		params: { protocolVersion: '2025-06-18', capabilities: {}, clientInfo: { name: 'curl', version: '1.0' } }
	};
	return [
		{ title: 'Set the token', where: 'Terminal', code: exportLine(token) },
		{
			title: 'Initialize',
			where: 'Terminal',
			code: head + shellQuote(JSON.stringify(init)),
			note: 'Expect a result with serverInfo and instructions. A 401 means the token is missing or wrong.'
		},
		{
			title: 'List the tools',
			where: 'Terminal',
			code: head + shellQuote(JSON.stringify({ jsonrpc: '2.0', id: 2, method: 'tools/list' })),
			note: 'The server is stateless, so tools/list works without a session.'
		}
	];
}

/** The MCP tools (crates/mcp/src/tools.rs `definitions()`), one line each. */
export const MCP_TOOLS: { name: string; description: string }[] = [
	{ name: 'search', description: "Search code across all repositories with Sourcegull's query language." },
	{ name: 'read_file', description: 'Read a file, or a line range of it, with line numbers.' },
	{ name: 'list_files', description: 'List a directory, or all files below it.' },
	{ name: 'find_symbols', description: 'Find where functions, types, constants … are defined, by name.' },
	{ name: 'go_to_definition', description: 'Jump from an identifier on a line to its definition.' },
	{ name: 'find_references', description: 'Find the usages of an identifier across repositories.' },
	{ name: 'find_implementations', description: 'Find the implementations of an interface or method, and what a type implements.' },
	{ name: 'list_repos', description: 'List the repositories you can search.' },
	{ name: 'list_branches', description: 'List the branches and tags of a repository.' },
	{ name: 'commit_log', description: 'Commit history of a revision or path, one line per commit.' },
	{ name: 'get_commit', description: 'Show a commit: message, author, changed files and diff.' },
	{ name: 'compare_revisions', description: 'Compare two revisions: commits and changed files.' },
	{ name: 'blame', description: 'Who last changed each line of a file.' }
];

/** Rules to paste into CLAUDE.md, AGENTS.md, .cursor/rules or Copilot instructions. */
export const RULES_SNIPPET = `## Code search (Sourcegull MCP)

This organisation's code is indexed by Sourcegull, available through the \`sourcegull\` MCP server.

- For anything beyond this repository (other services, shared libraries, callers of an API, how a config key is used elsewhere), use the Sourcegull tools instead of guessing or cloning repositories.
- Locate code first: use \`find_symbols\` for definitions by name and \`search\` for text, regexps and filters (\`repo:\`, \`file:\`, \`lang:\`, \`type:commit\`/\`type:diff\`).
- Before reading files, navigate: use \`go_to_definition\`, \`find_references\` and \`find_implementations\` on an identifier from a line you have already read.
- Read only what you need: call \`read_file\` with \`start_line\`/\`end_line\` around the hits, not whole files.
- Name repositories as \`group/project\` (a unique suffix of the full name works); use \`list_repos\` if unsure.
- For history, use \`commit_log\`, \`get_commit\`, \`compare_revisions\` and \`blame\`.
- When you report findings, cite them as \`group/project/path:line\`.`;

/** Example tasks to give an agent once Sourcegull is connected. */
export const EXAMPLE_PROMPTS: { title: string; prompt: string }[] = [
	{
		title: 'Callers across repositories',
		prompt: 'Using Sourcegull, find every caller of `parseConfig` across all repositories and summarize how it is used: which arguments are common, and which callers handle its errors differently.'
	},
	{
		title: 'Where a config key is read',
		prompt: 'Using Sourcegull, find where the config key `payments.retry_limit` is read, in code and in deployment files, and list each place with its default value.'
	},
	{
		title: "This week's changes",
		prompt: "Using Sourcegull, summarize what changed in `platform/billing` in the last 7 days: group the commits by topic and call out anything that changes an API or a database schema."
	},
	{
		title: 'Service to service',
		prompt: 'Using Sourcegull, explain how `orders-service` calls `inventory-service`: the client code, the endpoints or RPCs used, retries and timeouts, and the handler on the other side.'
	},
	{
		title: 'Similar implementations',
		prompt: 'Using Sourcegull, find other implementations of rate limiting across our repositories, compare their approaches, and recommend one to reuse here.'
	},
	{
		title: 'Impact of a change',
		prompt: 'I want to change the signature of `UserRepository.findByEmail`. Using Sourcegull, find all references in every repository and list what would need to change.'
	},
	{
		title: 'Onboarding tour',
		prompt: 'Using Sourcegull, give me a tour of `group/project`: the main entry points, how a request flows through it, and the five files I should read first, with line ranges.'
	},
	{
		title: 'Who knows this code',
		prompt: 'Using Sourcegull, look at the commit history and blame of `src/auth/` in `group/project` and tell me who has worked on it most recently and what the last few changes were about.'
	}
];
