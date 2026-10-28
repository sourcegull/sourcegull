import { describe, expect, it } from 'vitest';
import {
	agentGuides,
	exportLine,
	mcpEndpoint,
	MCP_TOOLS,
	oauthAvailable,
	oauthSnippets,
	shellQuote,
	testCommands,
	TOKEN_PLACEHOLDER
} from './connect.ts';

const URL = 'https://sourcegull.example.com/.api/mcp';

describe('mcpEndpoint', () => {
	it('appends the MCP path to the origin', () => {
		expect(mcpEndpoint('https://sourcegull.example.com')).toBe(URL);
		expect(mcpEndpoint('https://sourcegull.example.com/')).toBe(URL);
	});
});

describe('agentGuides', () => {
	const guides = agentGuides(URL, 'sgp_abc123');
	const byId = (id: string) => guides.find((g) => g.id === id)!;
	const snippet = (id: string, i: number) => byId(id).snippets[i]!;

	it('covers each agent once', () => {
		const ids = guides.map((g) => g.id);
		expect(new Set(ids).size).toBe(ids.length);
		expect(ids).toEqual([
			'claude-code',
			'cursor',
			'vscode',
			'windsurf',
			'codex',
			'gemini',
			'zed',
			'jetbrains',
			'cline',
			'continue',
			'other'
		]);
	});

	it('puts the endpoint in every guide', () => {
		for (const g of guides) expect(g.snippets.some((s) => s.code.includes(URL)), g.id).toBe(true);
	});

	it('emits valid JSON for JSON snippets', () => {
		for (const g of guides)
			for (const s of g.snippets)
				if (s.code.startsWith('{')) expect(() => JSON.parse(s.code), `${g.id}: ${s.title}`).not.toThrow();
	});

	it('builds the Claude Code command and project file', () => {
		const cmd = snippet('claude-code', 0);
		const file = snippet('claude-code', 1);
		expect(cmd.code).toBe(
			`claude mcp add --transport http --scope user sourcegull ${URL} \\\n  --header 'Authorization: Bearer sgp_abc123'`
		);
		expect(JSON.parse(file.code)).toEqual({
			mcpServers: { sourcegull: { type: 'http', url: URL, headers: { Authorization: 'Bearer ${SOURCEGULL_TOKEN}' } } }
		});
		expect(file.note).toContain("export SOURCEGULL_TOKEN='sgp_abc123'");
	});

	it('uses env references where clients support them', () => {
		expect(JSON.parse(snippet('cursor', 0).code).mcpServers.sourcegull.headers.Authorization).toBe(
			'Bearer ${env:SOURCEGULL_TOKEN}'
		);
		expect(JSON.parse(snippet('windsurf', 0).code).mcpServers.sourcegull.serverUrl).toBe(URL);
		expect(snippet('codex', 2).code).toBe(
			`[mcp_servers.sourcegull]\nurl = "${URL}"\nbearer_token_env_var = "SOURCEGULL_TOKEN"`
		);
		expect(snippet('continue', 0).code).toContain('${{ secrets.SOURCEGULL_TOKEN }}');
	});

	it('prompts for the token in VS Code', () => {
		const cfg = JSON.parse(snippet('vscode', 0).code);
		expect(cfg.inputs[0]).toMatchObject({ type: 'promptString', id: 'sourcegull-token', password: true });
		expect(cfg.servers.sourcegull.headers.Authorization).toBe('Bearer ${input:sourcegull-token}');
		expect(snippet('vscode', 0).code).not.toContain('sgp_abc123');
	});

	it('bridges stdio clients through mcp-remote without spaces in the header arg', () => {
		const cfg = JSON.parse(snippet('jetbrains', 0).code).mcpServers.sourcegull;
		expect(cfg.args).toEqual(['-y', 'mcp-remote', URL, '--header', 'Authorization:${AUTH_HEADER}']);
		expect(cfg.env).toEqual({ AUTH_HEADER: 'Bearer sgp_abc123' });
	});

	it('uses the placeholder without a token', () => {
		const zed = JSON.parse(agentGuides(URL)[6]!.snippets[0]!.code);
		expect(zed.context_servers.sourcegull.headers.Authorization).toBe(`Bearer ${TOKEN_PLACEHOLDER}`);
	});
});

describe('shell quoting', () => {
	it('escapes single quotes', () => {
		expect(shellQuote("a'b")).toBe(`'a'\\''b'`);
		expect(exportLine('t')).toBe("export SOURCEGULL_TOKEN='t'");
	});
});

describe('testCommands', () => {
	it('sends initialize and tools/list as JSON-RPC', () => {
		const [, init, list] = testCommands(URL, 'x') as [unknown, { code: string }, { code: string }];
		expect(init.code).toContain(`curl -sS ${URL}`);
		const body = (code: string) => JSON.parse(code.slice(code.lastIndexOf("-d '") + 4, -1));
		expect(body(init.code)).toMatchObject({ method: 'initialize', params: { protocolVersion: '2025-06-18' } });
		expect(body(list.code)).toEqual({ jsonrpc: '2.0', id: 2, method: 'tools/list' });
	});
});

describe('MCP_TOOLS', () => {
	it('lists the thirteen tools', () => {
		expect(MCP_TOOLS).toHaveLength(13);
	});
});

describe('oauthSnippets', () => {
	it('needs only the URL', () => {
		const snippets = oauthSnippets(URL);
		expect(snippets[0]!.code).toBe(`claude mcp add --transport http sourcegull ${URL}`);
		for (const s of snippets) {
			expect(s.code).toContain(URL);
			expect(s.code).not.toContain('Authorization');
		}
		expect(JSON.parse(snippets[1]!.code)).toEqual({ servers: { sourcegull: { type: 'http', url: URL } } });
	});

	it('detects OAuth from the metadata document', async () => {
		const ok = (status: number) => (async () => new Response('{}', { status })) as unknown as typeof fetch;
		expect(await oauthAvailable(ok(200))).toBe(true);
		expect(await oauthAvailable(ok(404))).toBe(false);
		const failing = (async () => {
			throw new Error('offline');
		}) as unknown as typeof fetch;
		expect(await oauthAvailable(failing)).toBe(false);
	});
});
