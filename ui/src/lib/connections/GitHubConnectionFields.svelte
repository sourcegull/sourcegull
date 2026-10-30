<!--
	The form view of a GitHub connection: URL, a token or GitHub App (with how to create one), which repositories
	to mirror, exclusions and the less common settings. It edits the parsed document through
	`onedit`; keys it doesn't show are left alone. List fields keep their own text and write it
	when they lose focus, so typing a new line isn't undone by the round trip.
-->
<script lang="ts">
	import { untrack } from 'svelte';
	import {
		AFFILIATED,
		appFieldEdit,
		authMethodEdits,
		classicTokenURL,
		exclusionsValue,
		fineGrainedTokenURL,
		githubBase,
		jsonOnlyKeys,
		normalizeOrg,
		newAppURL,
		normalizeRepo,
		readApp,
		readAuthMethod,
		readExclusions,
		readRepos,
		repoEdits,
		type AppFields,
		type AuthMethod,
		type RepoMode
	} from './github-form.ts';
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
	const base = $derived(githubBase(url));
	const host = $derived(base.replace(/^https?:\/\//, ''));
	const token = $derived(typeof doc.token === 'string' ? doc.token : '');
	const tokenStored = $derived(editing && token === 'REDACTED');
	const authMethod = $derived<AuthMethod>(readAuthMethod(doc));
	const app = $derived(readApp(doc));
	const keyStored = $derived(editing && app.privateKey === 'REDACTED');
	// The token to restore when switching back from GitHub App (the stored one when editing).
	const initialToken = untrack(() => (typeof doc.token === 'string' ? doc.token : PLACEHOLDER_TOKEN));
	function setAuthMethod(m: AuthMethod): void {
		onedit(authMethodEdits(m, initialToken));
	}
	function setAppField(field: keyof AppFields, raw: string): void {
		onedit([appFieldEdit(field, raw, editing)]);
	}
	const rateLimit = $derived(
		(doc.rateLimit && typeof doc.rateLimit === 'object' ? doc.rateLimit : {}) as Record<string, unknown>
	);
	const otherKeys = $derived(jsonOnlyKeys(doc));
	const syncsPerms = $derived.by(() => {
		const a = doc.authorization as { identityProvider?: { type?: unknown } } | undefined;
		return a?.identityProvider?.type === 'oauth';
	});
	function setAuthorization(on: boolean): void {
		onedit([[['authorization'], on ? { identityProvider: { type: 'oauth' } } : undefined]]);
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

	function writeRepos(): void {
		orgsText = lines(orgsText, normalizeOrg).join('\n');
		reposText = lines(reposText, normalizeRepo).join('\n');
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
		excludeNamesText = lines(excludeNamesText, normalizeRepo).join('\n');
		onedit([
			[
				['exclude'],
				exclusionsValue({
					names: lines(excludeNamesText),
					patterns: lines(excludePatternsText),
					archived: excludeArchived,
					forks: excludeForks,
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

<div class="github-fields">
	<section aria-labelledby="gh-instance">
		<h3 id="gh-instance">GitHub instance</h3>
		<div class="field">
			<label for="gh-url">GitHub URL</label>
			<input
				id="gh-url"
				class="input"
				type="url"
				placeholder="https://github.com"
				value={url}
				{disabled}
				oninput={(e) => onedit([[['url'], value(e).trim()]])}
			/>
			<span class="hint">https://github.com, or the address of a GitHub Enterprise Server (3.10 or later).</span>
		</div>
	</section>

	<section aria-labelledby="gh-token">
		<h3 id="gh-token">Authentication</h3>
		<fieldset class="modes" {disabled}>
			<legend class="visually-hidden">How Sourcegull authenticates to GitHub</legend>
			<label class="checkbox">
				<input type="radio" name="gh-auth-method" checked={authMethod === 'token'} onchange={() => setAuthMethod('token')} />
				<span>Personal access token</span>
			</label>
			<label class="checkbox">
				<input type="radio" name="gh-auth-method" checked={authMethod === 'app'} onchange={() => setAuthMethod('app')} />
				<span>GitHub App: installation tokens, higher rate limits, no user account</span>
			</label>
		</fieldset>
		{#if authMethod === 'token'}
			<details class="setup" open={!editing}>
				<summary>How to create the token</summary>
				<ol class="steps">
					<li>
						Sign in to <code class="code-inline">{host}</code> as the user whose access Sourcegull should have. Every
						repository the token can read can be mirrored, so a dedicated machine user is a good choice.
					</li>
					<li>
						Create a
						<a href={classicTokenURL(url)} target="_blank" rel="noopener noreferrer">classic token</a>
						with the <code class="code-inline">repo</code> scope (no scope for public repositories only). It reads the
						user's own repositories and those of every organization the user belongs to, which is what
						<em>Every repository the token's user owns…</em> below lists.
					</li>
					<li>
						Or, to limit access, create a
						<a href={fineGrainedTokenURL(url)} target="_blank" rel="noopener noreferrer">fine-grained token</a>
						with read-only <code class="code-inline">Contents</code> and <code class="code-inline">Metadata</code>
						permissions. A fine-grained token reads the repositories of one resource owner only (your account
						or one organization): to mirror several, add a connection per owner.
					</li>
					<li>Copy the token into the field below. GitHub shows it only once.</li>
					<li>
						Note the expiry date: when the token expires, syncing stops and the connection's page shows the error.
					</li>
				</ol>
			</details>
			<div class="field">
				<label for="gh-token-input">Token</label>
				<input
					id="gh-token-input"
					class="input mono"
					type="password"
					autocomplete="off"
					placeholder={tokenStored ? 'Stored token kept; type a new one to replace it' : 'ghp_…'}
					value={tokenStored || token === PLACEHOLDER_TOKEN ? '' : token}
					{disabled}
					oninput={(e) => setToken(value(e).trim())}
				/>
				<span class="hint">Stored encrypted. Use <em>Test connection</em> below to check it.</span>
			</div>
		{:else}
			<details class="setup" open={!editing}>
				<summary>How to set up the GitHub App</summary>
				<ol class="steps">
					<li>
						Open
						<a href={newAppURL(url)} target="_blank" rel="noopener noreferrer"
							>Settings › Developer settings › GitHub Apps › New GitHub App</a
						>
						(or the same page of the organization) and register an App. It needs no callback or webhook URL.
						<ul>
							<li>
								Repository permissions: <code class="code-inline">Contents</code> and
								<code class="code-inline">Metadata</code>, read-only.
							</li>
							<li>
								For repository permissions in Sourcegull: organization permission
								<code class="code-inline">Members</code>, read-only.
							</li>
						</ul>
					</li>
					<li>Note the App ID on the App's page, and generate a private key (a <code class="code-inline">.pem</code> file).</li>
					<li>
						Install the App on the organization or account, for all repositories or the ones to mirror. The
						installation ID is the number at the end of the installation's settings URL.
					</li>
				</ol>
			</details>
			<div class="app-ids">
				<div class="field">
					<label for="gh-app-id">App ID</label>
					<input
						id="gh-app-id"
						class="input mono"
						inputmode="numeric"
						placeholder="123456"
						value={app.appID === '0' ? '' : app.appID}
						{disabled}
						oninput={(e) => setAppField('appID', value(e))}
					/>
				</div>
				<div class="field">
					<label for="gh-app-installation">Installation ID</label>
					<input
						id="gh-app-installation"
						class="input mono"
						inputmode="numeric"
						placeholder="12345678"
						value={app.installationID === '0' ? '' : app.installationID}
						{disabled}
						oninput={(e) => setAppField('installationID', value(e))}
					/>
				</div>
			</div>
			<div class="field">
				<label for="gh-app-key">Private key (PEM)</label>
				<textarea
					id="gh-app-key"
					class="input mono"
					rows="4"
					autocomplete="off"
					placeholder={keyStored ? 'Stored key kept; paste a new one to replace it' : '-----BEGIN RSA PRIVATE KEY-----'}
					value={keyStored ? '' : app.privateKey}
					{disabled}
					onchange={(e) => setAppField('privateKey', (e.currentTarget as HTMLTextAreaElement).value)}
				></textarea>
				<span class="hint">
					Stored encrypted. In a configuration file, use <code class="code-inline">{'${file:/path/to/key.pem}'}</code>.
				</span>
			</div>
		{/if}
	</section>

	<section aria-labelledby="gh-repos">
		<h3 id="gh-repos">Repositories to mirror</h3>
		<fieldset class="modes" {disabled}>
			<legend class="visually-hidden">Which repositories</legend>
			<label class="checkbox">
				<input type="radio" name="gh-mode" checked={mode === 'affiliated'} onchange={() => setMode('affiliated')} />
				<span
					>{authMethod === 'app'
						? 'Every repository the App installation can access'
						: "Every repository the token's user owns, collaborates on or reaches through an organization"}</span
				>
			</label>
			<label class="checkbox">
				<input type="radio" name="gh-mode" checked={mode === 'selected'} onchange={() => setMode('selected')} />
				<span>Selected organizations and repositories</span>
			</label>
			<label class="checkbox">
				<input type="radio" name="gh-mode" checked={mode === 'custom'} onchange={() => setMode('custom')} />
				<span>Repository search queries</span>
			</label>
		</fieldset>

		{#if mode === 'custom'}
			<div class="field">
				<label for="gh-queries">Queries</label>
				<textarea
					id="gh-queries"
					class="input mono"
					rows="3"
					placeholder={'org:acme topic:backend\naffiliated'}
					bind:value={queriesText}
					{disabled}
					onchange={writeRepos}
				></textarea>
				<span class="hint">
					One per line: a GitHub repository search (at most 1,000 results each),
					<code class="code-inline">affiliated</code>, or <code class="code-inline">public</code> (every public
					repository; GitHub Enterprise Server only).
				</span>
			</div>
		{/if}
		{#if mode !== 'affiliated'}
			<div class="field">
				<label for="gh-orgs">Organizations</label>
				<textarea
					id="gh-orgs"
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
				<label for="gh-repos-input">Repositories</label>
				<textarea
					id="gh-repos-input"
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
			open={excludeNamesText !== '' || excludePatternsText !== '' || excludeArchived || excludeForks}
		>
			<summary>Exclusions</summary>
			<div class="field">
				<label for="gh-exclude-names">Skip these repositories</label>
				<textarea
					id="gh-exclude-names"
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
				<label for="gh-exclude-patterns">Skip repositories matching</label>
				<textarea
					id="gh-exclude-patterns"
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
		</details>
	</section>

	<section aria-labelledby="gh-perms">
		<h3 id="gh-perms">Repository permissions</h3>
		<div class="field">
			<label for="gh-auth">Who can see these repositories in Sourcegull</label>
			<select
				id="gh-auth"
				class="input"
				value={syncsPerms ? 'oauth' : 'none'}
				{disabled}
				onchange={(e) => setAuthorization(value(e) === 'oauth')}
			>
				<option value="none">Every Sourcegull user</option>
				<option value="oauth">Users who signed in with GitHub: what they can see on GitHub</option>
			</select>
			{#if syncsPerms}
				<span class="hint">
					Needs a GitHub sign-in provider for {host} in the
					<a href="/site-admin/configuration">site configuration</a>; sign-in then asks for the
					<code class="code-inline">repo</code> scope. Users who never signed in with GitHub see only public
					repositories. Per-repository syncs need a token with admin access to the repositories, or a GitHub App
					with the <code class="code-inline">Members</code> read permission.
				</span>
				<label class="checkbox">
					<input
						type="checkbox"
						checked={doc.markInternalReposAsPublic === true}
						{disabled}
						onchange={(e) =>
							onedit([
								[['markInternalReposAsPublic'], (e.currentTarget as HTMLInputElement).checked || undefined]
							])}
					/>
					Treat internal repositories (GitHub Enterprise) as public
				</label>
			{:else}
				<div class="alert alert-warning perm-alert" role="status">
					Every Sourcegull user can search every repository of this connection, including private ones. To apply GitHub
					permissions, set up <a href="/site-admin/configuration">sign-in with GitHub</a> and choose the second
					option.
				</div>
			{/if}
		</div>
	</section>

	<details class="sub advanced">
		<summary>Advanced</summary>
		<div class="field">
			<label for="gh-git-url">Clone over</label>
			<select
				id="gh-git-url"
				class="input"
				value={doc.gitURLType === 'ssh' ? 'ssh' : 'http'}
				{disabled}
				onchange={(e) => onedit([[['gitURLType'], value(e) === 'ssh' ? 'ssh' : undefined]])}
			>
				<option value="http">HTTPS, with the {authMethod === 'app' ? 'installation token' : 'token'}</option>
				<option value="ssh">SSH, with keys configured on the repo nodes</option>
			</select>
		</div>
		<div class="field">
			<label for="gh-rate">Requests per hour to GitHub</label>
			<input
				id="gh-rate"
				class="input narrow"
				type="number"
				min="0"
				placeholder="5000"
				value={typeof rateLimit.requestsPerHour === 'number' ? rateLimit.requestsPerHour : ''}
				{disabled}
				onchange={(e) => {
					const v = value(e).trim();
					onedit([[['rateLimit', 'requestsPerHour'], v === '' ? undefined : Number(v)]]);
				}}
			/>
			<span class="hint">Shared by every Sourcegull process. Empty: 5,000, GitHub's limit for a personal token.</span>
		</div>
		<div class="field">
			<label for="gh-path-pattern">Repository name pattern</label>
			<input
				id="gh-path-pattern"
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
			<label for="gh-cert">TLS certificate (PEM)</label>
			<textarea
				id="gh-cert"
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
			<span class="hint">Only for a GitHub Enterprise Server with a self-signed or internal certificate authority.</span>
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
	.github-fields {
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
	.app-ids {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(12rem, 1fr));
		gap: 0 var(--space-3);
	}
	.perm-alert {
		margin: 0;
		font-size: var(--text-sm);
	}
	.json-only {
		margin: 0 0 var(--space-3);
	}
</style>
