<!--
	The form view of a Gitea connection: URL, token (with how to create one), which repositories
	to mirror, exclusions and the less common settings. It edits the parsed document through
	`onedit`; keys it doesn't show are left alone. List fields keep their own text and write it
	when they lose focus, so typing a new line isn't undone by the round trip.
-->
<script lang="ts">
	import { untrack } from 'svelte';
	import {
		AFFILIATED,
		exclusionsValue,
		giteaBase,
		identityProvider,
		jsonOnlyKeys,
		normalizeOrg,
		normalizeRepo,
		readExclusions,
		readRepos,
		repoEdits,
		tokenURL,
		type RepoMode
	} from './gitea-form.ts';
	import { lines, type Edit } from './gitlab-form.ts';

	interface Props {
		doc: Record<string, unknown>;
		/** An existing connection: secrets read `REDACTED` and are kept unless replaced. */
		editing: boolean;
		disabled?: boolean;
		onedit: (edits: Edit[]) => void;
		/** Opens the JSON view. */
		onjson: () => void;
	}

	let { doc, editing, disabled = false, onedit, onjson }: Props = $props();

	const PLACEHOLDER_TOKEN = '<access token>';

	const url = $derived(typeof doc.url === 'string' ? doc.url : '');
	const host = $derived(giteaBase(url).replace(/^https?:\/\//, '').replace(/\/.*$/, ''));
	const token = $derived(typeof doc.token === 'string' ? doc.token : '');
	const tokenStored = $derived(editing && token === 'REDACTED');
	const rateLimit = $derived(
		(doc.rateLimit && typeof doc.rateLimit === 'object' ? doc.rateLimit : {}) as Record<string, unknown>
	);
	const otherKeys = $derived(jsonOnlyKeys(doc));
	const authType = $derived(identityProvider(doc));
	function setAuthorization(t: string): void {
		onedit([[['authorization'], t === 'none' ? undefined : { identityProvider: { type: t } }]]);
	}

	// The form's own copies of the list fields, seeded once from the document.
	const initialRepos = untrack(() => readRepos(doc));
	const initialExclusions = untrack(() => readExclusions(doc));
	let mode = $state<RepoMode>(initialRepos.mode);
	let orgsText = $state(initialRepos.orgs.join('\n'));
	let reposText = $state(initialRepos.repos.join('\n'));
	let queriesText = $state(initialRepos.queries.join('\n'));
	let excludeNamesText = $state(initialExclusions.names.join('\n'));
	let excludePatternsText = $state(initialExclusions.patterns.join('\n'));
	let excludeArchived = $state(initialExclusions.archived);
	let excludeForks = $state(initialExclusions.forks);
	let excludeMirrors = $state(initialExclusions.mirrors);

	function writeRepos(): void {
		orgsText = lines(orgsText, (s) => normalizeOrg(s, url)).join('\n');
		reposText = lines(reposText, (s) => normalizeRepo(s, url)).join('\n');
		onedit(
			repoEdits({
				mode,
				orgs: lines(orgsText),
				repos: lines(reposText),
				queries: lines(queriesText)
			})
		);
	}

	function setMode(m: RepoMode): void {
		if (m === 'custom' && mode !== 'custom') {
			// Start from what the other mode would store.
			queriesText = mode === 'affiliated' ? AFFILIATED : '';
		}
		mode = m;
		writeRepos();
	}

	function writeExclusions(): void {
		excludeNamesText = lines(excludeNamesText, (s) => normalizeRepo(s, url)).join('\n');
		onedit([
			[
				['exclude'],
				exclusionsValue({
					names: lines(excludeNamesText),
					patterns: lines(excludePatternsText),
					archived: excludeArchived,
					forks: excludeForks,
					mirrors: excludeMirrors,
					other: readExclusions(doc).other
				})
			]
		]);
	}

	function setToken(value: string): void {
		// An emptied field keeps the stored token.
		onedit([[['token'], value === '' && editing ? 'REDACTED' : value]]);
	}

	const value = (e: Event) => (e.currentTarget as HTMLInputElement).value;
</script>

<div class="gitea-fields">
	<section aria-labelledby="gt-instance">
		<h3 id="gt-instance">Gitea instance</h3>
		<div class="field">
			<label for="gt-url">Gitea URL</label>
			<input
				id="gt-url"
				class="input"
				type="url"
				placeholder="https://gitea.example.com"
				value={url}
				{disabled}
				oninput={(e) => onedit([[['url'], value(e).trim()]])}
			/>
			<span class="hint">
				The address of the Gitea instance (1.20 or later), with its path if it is served under one, e.g.
				<code class="code-inline">https://example.com/git</code>.
			</span>
		</div>
	</section>

	<section aria-labelledby="gt-token">
		<h3 id="gt-token">Access token</h3>
		<details class="setup" open={!editing}>
			<summary>How to create the token</summary>
			<ol class="steps">
				<li>
					Sign in to <code class="code-inline">{host}</code> as the user whose access Sourcegull should have. Every
					repository the token can read can be mirrored, so a dedicated machine user is a good choice.
				</li>
				<li>
					Open
					<a href={tokenURL(url)} target="_blank" rel="noopener noreferrer">Settings › Applications</a>
					and generate a token with read permissions for:
					<ul>
						<li><code class="code-inline">repository</code>: to list and clone repositories,</li>
						<li><code class="code-inline">organization</code>: to list the repositories of organizations,</li>
						<li><code class="code-inline">user</code>: for the connection test and the user's own repositories.</li>
					</ul>
				</li>
				<li>Copy the token into the field below. Gitea shows it only once.</li>
			</ol>
		</details>
		<div class="field">
			<label for="gt-token-input">Token</label>
			<input
				id="gt-token-input"
				class="input mono"
				type="password"
				autocomplete="off"
				placeholder={tokenStored ? 'Stored token kept; type a new one to replace it' : 'Access token'}
				value={tokenStored || token === PLACEHOLDER_TOKEN ? '' : token}
				{disabled}
				oninput={(e) => setToken(value(e).trim())}
			/>
			<span class="hint">Stored encrypted. Use <em>Test connection</em> below to check it.</span>
		</div>
	</section>

	<section aria-labelledby="gt-repos">
		<h3 id="gt-repos">Repositories to mirror</h3>
		<fieldset class="modes" {disabled}>
			<legend class="visually-hidden">Which repositories</legend>
			<label class="checkbox">
				<input type="radio" name="gt-mode" checked={mode === 'affiliated'} onchange={() => setMode('affiliated')} />
				<span
					>Every repository the token's user owns, collaborates on or reaches through an organization</span
				>
			</label>
			<label class="checkbox">
				<input type="radio" name="gt-mode" checked={mode === 'selected'} onchange={() => setMode('selected')} />
				<span>Selected organizations and repositories</span>
			</label>
			<label class="checkbox">
				<input type="radio" name="gt-mode" checked={mode === 'custom'} onchange={() => setMode('custom')} />
				<span>Repository searches</span>
			</label>
		</fieldset>

		{#if mode === 'custom'}
			<div class="field">
				<label for="gt-queries">Queries</label>
				<textarea
					id="gt-queries"
					class="input mono"
					rows="3"
					placeholder={'backend\naffiliated'}
					bind:value={queriesText}
					{disabled}
					onchange={writeRepos}
				></textarea>
				<span class="hint">
					One per line: a keyword matched against repository names,
					<code class="code-inline">affiliated</code>, or <code class="code-inline">all</code> (every repository
					the token can read, other users' public ones included; not on gitea.com).
				</span>
			</div>
		{/if}
		{#if mode !== 'affiliated'}
			<div class="field">
				<label for="gt-orgs">Organizations</label>
				<textarea
					id="gt-orgs"
					class="input mono"
					rows="2"
					placeholder={'acme\nacme-labs'}
					bind:value={orgsText}
					{disabled}
					onchange={writeRepos}
				></textarea>
				<span class="hint">One per line: an organization name or its URL. Every repository the token can read.</span>
			</div>
			<div class="field">
				<label for="gt-repos-input">Repositories</label>
				<textarea
					id="gt-repos-input"
					class="input mono"
					rows="3"
					placeholder={'acme/api\nacme/web'}
					bind:value={reposText}
					{disabled}
					onchange={writeRepos}
				></textarea>
				<span class="hint">One per line: <code class="code-inline">owner/name</code> or the repository's URL.</span>
			</div>
		{/if}

		<details
			class="sub"
			open={excludeNamesText !== '' || excludePatternsText !== '' || excludeArchived || excludeForks || excludeMirrors}
		>
			<summary>Exclusions</summary>
			<div class="field">
				<label for="gt-exclude-names">Skip these repositories</label>
				<textarea
					id="gt-exclude-names"
					class="input mono"
					rows="2"
					placeholder="acme/legacy-monolith"
					bind:value={excludeNamesText}
					{disabled}
					onchange={writeExclusions}
				></textarea>
				<span class="hint">One per line: <code class="code-inline">owner/name</code> or its URL.</span>
			</div>
			<div class="field">
				<label for="gt-exclude-patterns">Skip repositories matching</label>
				<textarea
					id="gt-exclude-patterns"
					class="input mono"
					rows="2"
					placeholder={'^acme/sandbox-\n-archive$'}
					bind:value={excludePatternsText}
					{disabled}
					onchange={writeExclusions}
				></textarea>
				<span class="hint">One regular expression per line, matched against <code class="code-inline">owner/name</code>.</span>
			</div>
			<label class="checkbox">
				<input type="checkbox" bind:checked={excludeArchived} {disabled} onchange={writeExclusions} />
				Skip archived repositories
			</label>
			<label class="checkbox">
				<input type="checkbox" bind:checked={excludeForks} {disabled} onchange={writeExclusions} />
				Skip forks
			</label>
			<label class="checkbox">
				<input type="checkbox" bind:checked={excludeMirrors} {disabled} onchange={writeExclusions} />
				Skip mirrors of other repositories
			</label>
		</details>
	</section>

	<section aria-labelledby="gt-perms">
		<h3 id="gt-perms">Repository permissions</h3>
		<div class="field">
			<label for="gt-auth">Who can see these repositories in Sourcegull</label>
			<select id="gt-auth" class="input" value={authType} {disabled} onchange={(e) => setAuthorization(value(e))}>
				<option value="none">Every Sourcegull user</option>
				<option value="oauth">Users who signed in with Gitea: what they can see on Gitea</option>
				<option value="username">Users matched by username: what they can see on Gitea (admin token)</option>
			</select>
			{#if authType === 'none'}
				<div class="alert alert-warning perm-alert" role="status">
					Every Sourcegull user can search every repository of this connection, including private ones. To apply Gitea
					permissions, set up <a href="/site-admin/configuration">sign-in with Gitea</a> and choose the second option,
					or use a site administrator's token and choose the third.
				</div>
			{:else if authType === 'oauth'}
				<span class="hint">
					Needs a Gitea sign-in provider for {host} in the
					<a href="/site-admin/configuration">site configuration</a>; sign-in then asks for the
					<code class="code-inline">read:repository</code> scope. Users who never signed in with Gitea see only public
					repositories.
				</span>
			{:else}
				<span class="hint">
					Sourcegull users are matched to Gitea users with the same username. The token must belong to a Gitea site
					administrator, which reads each user's repositories on their behalf (<code class="code-inline">Sudo</code>).
				</span>
			{/if}
		</div>
	</section>

	<details class="sub advanced">
		<summary>Advanced</summary>
		<div class="field">
			<label for="gt-git-url">Clone over</label>
			<select
				id="gt-git-url"
				class="input"
				value={doc.gitURLType === 'ssh' ? 'ssh' : 'http'}
				{disabled}
				onchange={(e) => onedit([[['gitURLType'], value(e) === 'ssh' ? 'ssh' : undefined]])}
			>
				<option value="http">HTTPS, with the token</option>
				<option value="ssh">SSH, with keys configured on the repo nodes</option>
			</select>
		</div>
		<div class="field">
			<label for="gt-rate">Requests per hour to Gitea</label>
			<input
				id="gt-rate"
				class="input narrow"
				type="number"
				min="0"
				placeholder="36000"
				value={typeof rateLimit.requestsPerHour === 'number' ? rateLimit.requestsPerHour : ''}
				{disabled}
				onchange={(e) => {
					const v = value(e).trim();
					onedit([[['rateLimit', 'requestsPerHour'], v === '' ? undefined : Number(v)]]);
				}}
			/>
			<span class="hint">Shared by every Sourcegull process. Empty: 36,000 (10 per second).</span>
		</div>
		<div class="field">
			<label for="gt-path-pattern">Repository name pattern</label>
			<input
				id="gt-path-pattern"
				class="input mono"
				placeholder={'{host}/{nameWithOwner}'}
				value={typeof doc.repositoryPathPattern === 'string' ? doc.repositoryPathPattern : ''}
				{disabled}
				onchange={(e) => {
					const v = value(e).trim();
					onedit([[['repositoryPathPattern'], v === '' ? undefined : v]]);
				}}
			/>
			<span class="hint">
				How repositories are named in Sourcegull, e.g. <code class="code-inline">{host}/acme/api</code> by default.
			</span>
		</div>
		<div class="field cert">
			<label for="gt-cert">TLS certificate (PEM)</label>
			<textarea
				id="gt-cert"
				class="input mono"
				rows="3"
				placeholder="-----BEGIN CERTIFICATE-----"
				value={typeof doc.certificate === 'string' ? doc.certificate : ''}
				{disabled}
				onchange={(e) => {
					const v = value(e).trim();
					onedit([[['certificate'], v === '' ? undefined : v + '\n']]);
				}}
			></textarea>
			<span class="hint">Only for an instance with a self-signed or internal certificate authority.</span>
		</div>
	</details>

	{#if otherKeys.length > 0}
		<p class="hint json-only">
			Also set in the JSON: {#each otherKeys as k, i (k)}{i > 0 ? ', ' : ''}<code class="code-inline">{k}</code>{/each}.
			<button type="button" class="btn btn-link" onclick={onjson}>Edit as JSON</button>
		</p>
	{/if}
</div>

<style>
	.gitea-fields {
		max-width: 48rem;
	}
	section {
		margin-bottom: var(--space-4);
	}
	h3 {
		margin: 0 0 var(--space-3);
		font-size: var(--text-md);
	}
	.mono {
		font-family: var(--font-mono);
		font-size: var(--text-sm);
	}
	.narrow {
		max-width: 12rem;
	}
	.setup,
	.sub {
		background: var(--color-bg-subtle);
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
		padding: var(--space-2) var(--space-4);
		margin-bottom: var(--space-3);
		font-size: var(--text-sm);
	}
	.sub {
		background: none;
	}
	.sub[open] {
		padding-bottom: var(--space-3);
	}
	.setup summary,
	.sub summary {
		cursor: pointer;
		font-weight: var(--weight-medium);
		padding: var(--space-1) 0;
	}
	.sub[open] summary {
		margin-bottom: var(--space-3);
	}
	.sub .checkbox {
		margin-bottom: var(--space-3);
	}
	.steps {
		margin: var(--space-2) 0;
		padding-left: var(--space-5);
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
	}
	.steps ul {
		margin: var(--space-1) 0;
		padding-left: var(--space-4);
	}
	.steps li {
		overflow-wrap: anywhere;
	}
	.modes {
		border: none;
		padding: 0;
		margin: 0 0 var(--space-3);
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
	}
	.perm-alert {
		margin: 0;
		font-size: var(--text-sm);
	}
	.json-only {
		margin: 0 0 var(--space-3);
	}
</style>
