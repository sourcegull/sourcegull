<script lang="ts">
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		CREATE_ACCESS_TOKEN,
		SCOPE_USER_ALL,
		type CreateAccessTokenResult,
		type CreateAccessTokenVariables
	} from '#lib/queries.ts';
	import {
		agentGuides,
		EXAMPLE_PROMPTS,
		MCP_TOOLS,
		mcpEndpoint,
		oauthAvailable,
		oauthSnippets,
		RULES_SNIPPET,
		testCommands,
		TOKEN_ENV,
		TOKEN_PLACEHOLDER
	} from '#lib/agents/connect.ts';
	import CodeSnippet from '#lib/components/CodeSnippet.svelte';
	import CopyField from '#lib/components/site-config/CopyField.svelte';
	import { onMount } from 'svelte';
	import type { PageProps } from './$types';

	let { data }: PageProps = $props();
	const user = $derived(data.auth.user);

	// Non-admins cannot read `externalURL`; the page is served from the instance, so its
	// origin is the URL agents should use.
	const url = mcpEndpoint(window.location.origin);

	/** The token created on this page: shown once, kept only in memory. */
	let token = $state<string | null>(null);
	let creating = $state(false);
	let tokenError = $state<string | null>(null);

	/** Whether agents can sign in through OAuth (`mcp.oauth.enabled`). */
	let oauth = $state(false);
	onMount(async () => {
		oauth = await oauthAvailable();
	});

	/** How the agent signs in: through the browser (OAuth) or with an access token. */
	let method = $state<'oauth' | 'token'>('oauth');
	const useToken = $derived(!oauth || method === 'token');

	let selected = $state('claude-code');
	const guides = $derived(agentGuides(url, token ?? TOKEN_PLACEHOLDER));
	const guide = $derived(guides.find((g) => g.id === selected) ?? guides[0]!);
	const tests = $derived(testCommands(url, token ?? TOKEN_PLACEHOLDER));

	async function createToken(): Promise<void> {
		if (!user) return;
		creating = true;
		tokenError = null;
		try {
			const result = await graphql<CreateAccessTokenResult, CreateAccessTokenVariables>(CREATE_ACCESS_TOKEN, {
				user: user.id,
				scopes: [SCOPE_USER_ALL],
				note: `Sourcegull MCP (${guide.label})`
			});
			token = result.createAccessToken.token;
		} catch (err) {
			tokenError = errorMessage(err);
		} finally {
			creating = false;
		}
	}

	function onTabKey(event: KeyboardEvent, index: number): void {
		const step = event.key === 'ArrowRight' ? 1 : event.key === 'ArrowLeft' ? -1 : 0;
		if (!step) return;
		event.preventDefault();
		const next = guides[(index + step + guides.length) % guides.length];
		if (!next) return;
		selected = next.id;
		document.getElementById(`agent-tab-${next.id}`)?.focus();
	}
</script>

<svelte:head><title>Connect an agent · Sourcegull</title></svelte:head>

<div class="page connect">
	<div class="page-header">
		<div>
			<h1>Connect an agent</h1>
			<p class="page-description">
				Give AI coding agents such as Claude Code, Cursor or Copilot access to Sourcegull's code search and
				navigation through the Model Context Protocol (MCP). The agent sees exactly the repositories you can.
			</p>
		</div>
	</div>

	{#if oauth}
		<fieldset class="methods">
			<legend><h2>How should the agent sign in?</h2></legend>
			<p class="muted">Choose one. Either way the agent sees only the repositories you can.</p>
			<label class="method" class:active={method === 'oauth'}>
				<input type="radio" name="method" value="oauth" bind:group={method} />
				<span>
					<strong>Through the browser</strong> <span class="badge">Recommended</span>
					<span class="small muted method-text">
						For Claude Code, VS Code and Cursor. Add only the server URL; the agent opens the browser for you to
						allow access. No token to copy, store or rotate.
					</span>
				</span>
			</label>
			<label class="method" class:active={method === 'token'}>
				<input type="radio" name="method" value="token" bind:group={method} />
				<span>
					<strong>With an access token</strong>
					<span class="small muted method-text">
						For other agents, CI jobs and scripts, or when the agent can't open a browser. You create a token and
						put it in the agent's configuration.
					</span>
				</span>
			</label>
		</fieldset>
	{/if}

	{#if !useToken}
		<section aria-labelledby="step-oauth">
			<div class="section-header">
				<h2 id="step-oauth"><span class="step">1</span> Add Sourcegull to your agent</h2>
			</div>
			<p class="muted">
				The first time the agent connects, it opens the browser: sign in and allow access. You can see and revoke
				the agents you allowed under <a href="/user/settings/connected-apps">Connected apps</a>.
			</p>
			{#each oauthSnippets(url) as s (s.title)}
				<div class="snippet-block">
					<h3>{s.title}</h3>
					<p class="where small muted">{s.where}</p>
					<CodeSnippet code={s.code} label="{s.title}: OAuth" />
					{#if s.note}<p class="hint small muted">{s.note}</p>{/if}
				</div>
			{/each}
			<p class="small muted">
				Agent not listed, or it doesn't open the browser?
				<button class="btn btn-link" type="button" onclick={() => (method = 'token')}>Use an access token</button>
				instead.
			</p>
		</section>
	{:else}
		<section aria-labelledby="step-url">
			<div class="section-header"><h2 id="step-url"><span class="step">1</span> MCP server URL</h2></div>
			<p class="muted">Streamable HTTP transport. Authenticate with an access token in the Authorization header.</p>
			<CopyField value={url} label="MCP server URL" />
		</section>

		<section aria-labelledby="step-token">
			<div class="section-header"><h2 id="step-token"><span class="step">2</span> Access token</h2></div>
			{#if token}
				<div class="alert alert-success" role="status">
					<strong>Copy your token now. You won't be able to see it again.</strong> It is filled into every snippet
					below until you leave this page.
					<CopyField value={token} label="Access token" />
				</div>
			{:else if user}
				<p class="muted">
					Creates a token with your access, noted "Sourcegull MCP ({guide.label})". Manage or revoke it under
					<a href="/user/settings/tokens">Access tokens</a>.
				</p>
				<div class="toolbar">
					<button class="btn btn-primary" type="button" onclick={createToken} disabled={creating}>
						{creating ? 'Creating…' : 'Create access token'}
					</button>
				</div>
				{#if tokenError}<div class="alert alert-danger" role="alert">{tokenError}</div>{/if}
			{:else}
				<p class="muted">
					<a href="/sign-in?returnTo=/connect-agent">Sign in</a> to create an access token; the snippets use
					<code class="code-inline">{TOKEN_PLACEHOLDER}</code> until then.
				</p>
			{/if}
			<div class="alert alert-warning">
				The token acts as you: anyone who has it can read all code you can. Keep it out of committed files (the
				snippets read it from <code class="code-inline">{TOKEN_ENV}</code> or a secret prompt where the agent
				supports that) and delete it when you no longer need it.
			</div>
		</section>

		<section aria-labelledby="step-agent">
			<div class="section-header"><h2 id="step-agent"><span class="step">3</span> Configure your agent</h2></div>
			<div class="tabs agent-tabs" role="tablist" aria-label="Agent">
				{#each guides as g, i (g.id)}
					<button
						class="tab"
						type="button"
						role="tab"
						id="agent-tab-{g.id}"
						aria-selected={g.id === guide.id}
						aria-controls="agent-panel"
						tabindex={g.id === guide.id ? 0 : -1}
						onclick={() => (selected = g.id)}
						onkeydown={(e) => onTabKey(e, i)}
					>
						{g.label}
					</button>
				{/each}
			</div>
			<div id="agent-panel" role="tabpanel" aria-labelledby="agent-tab-{guide.id}" class="panel">
				{#if guide.intro}<p>{guide.intro}</p>{/if}
				{#each guide.snippets as s (s.title)}
					<div class="snippet-block">
						<h3>{s.title}</h3>
						<p class="where small muted">{s.where}</p>
						<CodeSnippet code={s.code} label="{guide.label}: {s.title}" />
						{#if s.note}<p class="hint small muted">{s.note}</p>{/if}
					</div>
				{/each}
				<p class="small muted">
					Formats change between releases; see the
					<a href={guide.docs} target="_blank" rel="noopener noreferrer">{guide.label} MCP documentation</a>.
				</p>
			</div>
		</section>
	{/if}

	<section aria-labelledby="step-rules">
		<div class="section-header"><h2 id="step-rules"><span class="step">{useToken ? 4 : 2}</span> Tell the agent when to use Sourcegull</h2></div>
		<p class="muted">
			Paste into <code class="code-inline">CLAUDE.md</code>, <code class="code-inline">AGENTS.md</code>,
			<code class="code-inline">.cursor/rules</code>, <code class="code-inline">.github/copilot-instructions.md</code>
			or your agent's rules file.
		</p>
		<CodeSnippet code={RULES_SNIPPET} label="agent rules" wrap />
	</section>

	<section aria-labelledby="prompts">
		<div class="section-header"><h2 id="prompts">Example prompts</h2></div>
		<p class="muted">Replace the names in backticks with your own.</p>
		<ul class="prompts">
			{#each EXAMPLE_PROMPTS as p (p.title)}
				<li>
					<h3>{p.title}</h3>
					<CodeSnippet code={p.prompt} label="prompt: {p.title}" wrap />
				</li>
			{/each}
		</ul>
	</section>

	{#if useToken}
		<section aria-labelledby="verify">
			<div class="section-header"><h2 id="verify">Test the connection</h2></div>
			{#each tests as s (s.title)}
				<div class="snippet-block">
					<h3>{s.title}</h3>
					<CodeSnippet code={s.code} label="test: {s.title}" />
					{#if s.note}<p class="hint small muted">{s.note}</p>{/if}
				</div>
			{/each}
		</section>
	{/if}

	<section aria-labelledby="tools">
		<div class="section-header"><h2 id="tools">Available tools</h2></div>
		<p class="muted">All tools are read-only and run with your repository permissions.</p>
		<div class="table-scroll">
			<table class="table table-bordered table-sm">
				<thead>
					<tr><th scope="col">Tool</th><th scope="col">What it does</th></tr>
				</thead>
				<tbody>
					{#each MCP_TOOLS as t (t.name)}
						<tr><td><code class="code-inline">{t.name}</code></td><td>{t.description}</td></tr>
					{/each}
				</tbody>
			</table>
		</div>
	</section>
</div>

<style>
	.connect {
		max-width: 56rem;
	}
	section + section,
	.methods + section {
		margin-top: var(--space-8);
	}
	.methods {
		border: none;
		padding: 0;
		margin: 0;
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(min(100%, 18rem), 1fr));
		gap: var(--space-3);
	}
	.methods legend {
		padding: 0;
	}
	.methods legend h2 {
		margin: 0 0 var(--space-1);
	}
	.methods > p {
		grid-column: 1 / -1;
		margin: 0;
	}
	.method {
		display: flex;
		align-items: flex-start;
		gap: var(--space-2);
		padding: var(--space-3) var(--space-4);
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
		cursor: pointer;
	}
	.method:hover {
		background: var(--color-bg-subtle);
	}
	.method.active {
		border-color: var(--color-primary);
		box-shadow: 0 0 0 1px var(--color-primary);
	}
	.method input {
		margin-top: 0.2rem;
	}
	.method-text {
		display: block;
		margin-top: var(--space-1);
	}
	.section-header h2 {
		display: flex;
		align-items: center;
		gap: var(--space-2);
	}
	.step {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		width: 1.5rem;
		height: 1.5rem;
		flex: none;
		border-radius: var(--radius-full);
		background: var(--color-primary-subtle);
		color: var(--color-info);
		font-size: var(--text-xs);
		font-weight: var(--weight-semibold);
	}
	section > p {
		margin: 0 0 var(--space-3);
	}
	.alert {
		margin-top: var(--space-3);
	}
	.toolbar {
		margin-bottom: var(--space-2);
	}
	.agent-tabs {
		flex-wrap: wrap;
		overflow-x: visible;
	}
	.panel > p {
		margin: 0 0 var(--space-3);
	}
	.snippet-block {
		margin-bottom: var(--space-4);
	}
	h3 {
		margin: 0 0 var(--space-1);
		font-size: var(--text-md);
		font-weight: var(--weight-semibold);
	}
	.where {
		margin: 0 0 var(--space-2);
		overflow-wrap: anywhere;
	}
	.hint {
		margin: var(--space-2) 0 0;
		overflow-wrap: anywhere;
	}
	.prompts {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(min(100%, 22rem), 1fr));
		gap: var(--space-4);
	}
	td code {
		white-space: nowrap;
	}
</style>
