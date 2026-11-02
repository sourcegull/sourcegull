<!--
	The form view of a GitLab connection: URL, token (with how to create one), which projects to
	mirror, exclusions, permissions and the less common settings. It edits the parsed document
	through `onedit`; keys it doesn't show are left alone. List fields keep their own text and
	write it when they lose focus, so typing a new line isn't undone by the round trip.
-->
<script lang="ts">
	import { configSectionURL } from '#lib/admin/related.ts';
	import { untrack } from 'svelte';
	import CodeSnippet from '#lib/components/CodeSnippet.svelte';
	import {
		MEMBERSHIP_QUERY,
		exclusionsValue,
		gitlabBase,
		jsonOnlyKeys,
		lines,
		normalizePath,
		OAUTH_REDIRECT,
		oauthAuthorizeURL,
		oauthTokenCommand,
		projectEdits,
		readExclusions,
		readProjects,
		tokenPageURL,
		tokenTypeEdits,
		type Edit,
		type ProjectMode
	} from './gitlab-form.ts';

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
	const base = $derived(gitlabBase(url));
	const host = $derived(base.replace(/^https?:\/\//, ''));
	const token = $derived(typeof doc.token === 'string' ? doc.token : '');
	const tokenStored = $derived(editing && token === 'REDACTED');
	const tokenType = $derived(doc['token.type'] === 'oauth' ? 'oauth' : 'pat');
	const refresh = $derived(typeof doc['token.oauth.refresh'] === 'string' ? doc['token.oauth.refresh'] : '');
	const refreshStored = $derived(editing && refresh === 'REDACTED');
	const authType = $derived.by(() => {
		const a = doc.authorization as { identityProvider?: { type?: unknown } } | undefined;
		const t = a?.identityProvider?.type;
		return t === 'oauth' || t === 'username' ? t : 'none';
	});
	const rateLimit = $derived(
		(doc.rateLimit && typeof doc.rateLimit === 'object' ? doc.rateLimit : {}) as Record<string, unknown>
	);
	const otherKeys = $derived(jsonOnlyKeys(doc));

	// The form's own copies of the list fields, seeded once from the document.
	const initialProjects = untrack(() => readProjects(doc));
	const initialExclusions = untrack(() => readExclusions(doc));
	let mode = $state<ProjectMode>(initialProjects.mode);
	let groupsText = $state(initialProjects.groups.join('\n'));
	let projectsText = $state(initialProjects.projects.join('\n'));
	let queriesText = $state(initialProjects.queries.join('\n'));
	let excludeNamesText = $state(initialExclusions.names.join('\n'));
	let excludePatternsText = $state(initialExclusions.patterns.join('\n'));
	let excludeEmpty = $state(initialExclusions.emptyRepos);
	/** Whether the token will also be used for bulk changes (asks for `api`). */
	let wantsApi = $state(false);
	const scopes = $derived(wantsApi ? ['api'] : ['read_api']);
	/** The OAuth application's ID, only used to fill in the link and the command. */
	let clientID = $state('');

	function writeProjects(): void {
		groupsText = lines(groupsText, normalizePath).join('\n');
		projectsText = lines(projectsText, normalizePath).join('\n');
		onedit(
			projectEdits({
				mode,
				groups: lines(groupsText),
				projects: lines(projectsText),
				queries: lines(queriesText)
			})
		);
	}

	function setMode(m: ProjectMode): void {
		if (m === 'custom' && mode !== 'custom') {
			// Start from what the other mode would store.
			const current = readProjects(doc);
			queriesText = (mode === 'membership' ? [MEMBERSHIP_QUERY] : current.queries).join('\n');
		}
		mode = m;
		writeProjects();
	}

	function writeExclusions(): void {
		excludeNamesText = lines(excludeNamesText, normalizePath).join('\n');
		onedit([
			[
				['exclude'],
				exclusionsValue({
					names: lines(excludeNamesText),
					patterns: lines(excludePatternsText),
					emptyRepos: excludeEmpty,
					other: readExclusions(doc).other
				})
			]
		]);
	}

	function setToken(value: string): void {
		// An emptied field keeps the stored token.
		onedit([[['token'], value === '' && editing ? 'REDACTED' : value]]);
	}

	function setRefresh(value: string): void {
		onedit([[['token.oauth.refresh'], value === '' ? (refreshStored ? 'REDACTED' : undefined) : value]]);
	}

	function setAuthorization(t: string): void {
		onedit([[['authorization'], t === 'none' ? undefined : { identityProvider: { type: t } }]]);
	}

	const value = (e: Event) => (e.currentTarget as HTMLInputElement).value;
	const checked = (e: Event) => (e.currentTarget as HTMLInputElement).checked;
</script>

<div class="gitlab-fields">
	<section aria-labelledby="gl-instance">
		<h3 id="gl-instance">GitLab instance</h3>
		<div class="field">
			<label for="gl-url">GitLab URL</label>
			<input
				id="gl-url"
				class="input"
				type="url"
				placeholder="https://gitlab.com"
				value={url}
				{disabled}
				oninput={(e) => onedit([[['url'], value(e).trim()]])}
			/>
			<span class="hint">https://gitlab.com, or the address of a self-managed GitLab.</span>
		</div>
	</section>

	<section aria-labelledby="gl-token">
		<h3 id="gl-token">Access token</h3>
		<fieldset class="modes" {disabled}>
			<legend class="visually-hidden">Token type</legend>
			<label class="checkbox">
				<input
					type="radio"
					name="gl-token-type"
					checked={tokenType === 'pat'}
					onchange={() => onedit(tokenTypeEdits('pat'))}
				/>
				<span>
					Personal, group or project access token <span class="muted">(recommended)</span>
					<span class="hint block">Created in GitLab in a minute; it lasts until the expiry date you pick.</span>
				</span>
			</label>
			<label class="checkbox">
				<input
					type="radio"
					name="gl-token-type"
					checked={tokenType === 'oauth'}
					onchange={() => onedit(tokenTypeEdits('oauth'))}
				/>
				<span>
					OAuth token
					<span class="hint block">
						For a GitLab that doesn't allow access tokens. Issued through the OAuth application of
						<a href={configSectionURL('auth')}>sign-in with GitLab</a>; it lasts two hours and Sourcegull renews it with
						its refresh token.
					</span>
				</span>
			</label>
		</fieldset>
		<details class="setup" open={!editing}>
			<summary>How to create the token</summary>
			<ol class="steps">
				<li>
					Sign in to <code class="code-inline">{host}</code> as the user whose access Sourcegull should have. Every
					project that user can see can be mirrored, so a dedicated service account{tokenType === 'pat'
						? ' (or a group access token)'
						: ''} is a good choice.
				</li>
				<li>
					<label class="checkbox inline">
						<input type="checkbox" bind:checked={wantsApi} />
						I'll also use this connection for bulk changes (merge requests)
					</label>
				</li>
				{#if tokenType === 'pat'}
					<li>
						Open
						<a href={tokenPageURL(url, scopes)} target="_blank" rel="noopener noreferrer"
							>User settings › Access tokens</a
						>
						(the name and scope are filled in), pick an expiry date, and create the token. Scope:
						<ul>
							<li>
								<code class="code-inline">read_api</code>: search, code navigation and repository permissions.
							</li>
							<li>
								<code class="code-inline">api</code>: also merge requests for bulk changes and webhook registration.
							</li>
						</ul>
						A group or project access token with the same scope works too (Group › Settings › Access tokens); it sees only
						that group's projects.
					</li>
					<li>Copy the token into the field below. GitLab shows it only once.</li>
					<li>
						When the token expires, syncing stops. Sourcegull emails the site admins 14, 7 and 1 days before (<code
							class="code-inline">codeHosts.tokenExpiryAlerts</code
						>).
					</li>
				{:else}
					<li>
						Set up <a href={configSectionURL('auth')}>sign-in with GitLab</a> for
						<code class="code-inline">{host}</code> (an <code class="code-inline">auth.providers</code> entry with the same
						URL). Sourcegull renews the token with that entry's OAuth application.
					</li>
					<li>
						In GitLab, edit that OAuth application: add <code class="code-inline">{OAUTH_REDIRECT}</code> as another
						redirect URI (one per line) and tick the scope <code class="code-inline">{scopes[0]}</code>.
					</li>
					<li>
						<label for="gl-client-id">Its Application ID:</label>
						<input
							id="gl-client-id"
							class="input mono inline-input"
							autocomplete="off"
							placeholder="Application ID"
							bind:value={clientID}
						/>
						<span class="muted">(only fills in the link and the command; not saved)</span>
					</li>
					<li>
						Open <a href={oauthAuthorizeURL(url, clientID.trim(), scopes)} target="_blank" rel="noopener noreferrer"
							>the authorization page</a
						>
						and click <em>Authorize</em>. The browser then goes to
						<code class="code-inline">{OAUTH_REDIRECT}?code=…</code>, which doesn't load; copy the
						<code class="code-inline">code</code> from the address bar. It is valid for ten minutes.
					</li>
					<li>
						Exchange it for tokens, with the application's secret and the code:
						<CodeSnippet code={oauthTokenCommand(url, clientID.trim())} label="token command" />
					</li>
					<li>
						Copy <code class="code-inline">access_token</code> into <em>Token</em> and
						<code class="code-inline">refresh_token</code> into <em>Refresh token</em> below.
					</li>
				{/if}
			</ol>
		</details>
		<div class="field">
			<label for="gl-token-input">Token</label>
			<input
				id="gl-token-input"
				class="input mono"
				type="password"
				autocomplete="off"
				placeholder={tokenStored
					? 'Stored token kept; type a new one to replace it'
					: tokenType === 'oauth'
						? 'access_token'
						: 'glpat-…'}
				value={tokenStored || token === PLACEHOLDER_TOKEN ? '' : token}
				{disabled}
				oninput={(e) => setToken(value(e).trim())}
			/>
			<span class="hint">Stored encrypted. Use <em>Test connection</em> below to check it.</span>
		</div>
		{#if tokenType === 'oauth'}
			<div class="field">
				<label for="gl-refresh-input">Refresh token</label>
				<input
					id="gl-refresh-input"
					class="input mono"
					type="password"
					autocomplete="off"
					placeholder={refreshStored ? 'Stored refresh token kept; type a new one to replace it' : 'refresh_token'}
					value={refreshStored ? '' : refresh}
					{disabled}
					oninput={(e) => setRefresh(value(e).trim())}
				/>
				<span class="hint">
					Sourcegull stores each renewed token here. If GitLab rejects it (revoked, or used elsewhere), syncing stops
					until you create new tokens.
				</span>
			</div>
		{/if}
	</section>

	<section aria-labelledby="gl-projects">
		<h3 id="gl-projects">Projects to mirror</h3>
		<fieldset class="modes" {disabled}>
			<legend class="visually-hidden">Which projects</legend>
			<label class="checkbox">
				<input type="radio" name="gl-mode" checked={mode === 'membership'} onchange={() => setMode('membership')} />
				<span>Every project the token's user is a member of <span class="muted">(archived ones skipped)</span></span>
			</label>
			<label class="checkbox">
				<input type="radio" name="gl-mode" checked={mode === 'selected'} onchange={() => setMode('selected')} />
				<span>Selected groups and projects</span>
			</label>
			<label class="checkbox">
				<input type="radio" name="gl-mode" checked={mode === 'custom'} onchange={() => setMode('custom')} />
				<span>Custom API queries</span>
			</label>
		</fieldset>

		{#if mode === 'selected'}
			<div class="field">
				<label for="gl-groups">Groups</label>
				<textarea
					id="gl-groups"
					class="input mono"
					rows="3"
					placeholder={'acme\nacme/platform'}
					bind:value={groupsText}
					{disabled}
					onchange={writeProjects}
				></textarea>
				<span class="hint"
					>One per line: a group path or its URL. Subgroups are included, archived projects skipped.</span
				>
			</div>
		{:else if mode === 'custom'}
			<div class="field">
				<label for="gl-queries">API queries</label>
				<textarea
					id="gl-queries"
					class="input mono"
					rows="3"
					placeholder={'projects?membership=true&archived=no\ngroups/acme/projects?with_shared=false'}
					bind:value={queriesText}
					{disabled}
					onchange={writeProjects}
				></textarea>
				<span class="hint">
					One per line: GitLab API paths that list projects, relative to <code class="code-inline">/api/v4/</code>.
				</span>
			</div>
		{/if}
		{#if mode !== 'membership'}
			<div class="field">
				<label for="gl-projects-input">Projects</label>
				<textarea
					id="gl-projects-input"
					class="input mono"
					rows="3"
					placeholder={'acme/api\nacme/platform/web'}
					bind:value={projectsText}
					{disabled}
					onchange={writeProjects}
				></textarea>
				<span class="hint">One per line: a full project path, its URL, or a numeric project id.</span>
			</div>
		{/if}

		<details class="sub" open={excludeNamesText !== '' || excludePatternsText !== ''}>
			<summary>Exclusions</summary>
			<div class="field">
				<label for="gl-exclude-names">Skip these projects</label>
				<textarea
					id="gl-exclude-names"
					class="input mono"
					rows="2"
					placeholder="acme/legacy-monolith"
					bind:value={excludeNamesText}
					{disabled}
					onchange={writeExclusions}
				></textarea>
				<span class="hint">One per line: a full project path or its URL.</span>
			</div>
			<div class="field">
				<label for="gl-exclude-patterns">Skip projects matching</label>
				<textarea
					id="gl-exclude-patterns"
					class="input mono"
					rows="2"
					placeholder={'^acme/sandbox/\n-archive$'}
					bind:value={excludePatternsText}
					{disabled}
					onchange={writeExclusions}
				></textarea>
				<span class="hint">One regular expression per line, matched against the full project path.</span>
			</div>
			<label class="checkbox">
				<input type="checkbox" bind:checked={excludeEmpty} {disabled} onchange={writeExclusions} />
				Skip empty projects (no commits)
			</label>
		</details>
	</section>

	<section aria-labelledby="gl-perms">
		<h3 id="gl-perms">Repository permissions</h3>
		<div class="field">
			<label for="gl-auth">Who can see these repositories in Sourcegull</label>
			<select id="gl-auth" class="input" value={authType} {disabled} onchange={(e) => setAuthorization(value(e))}>
				<option value="none">Every Sourcegull user</option>
				<option value="oauth">Users who signed in with GitLab: what they can see in GitLab</option>
				<option value="username">Users matched by username: what they can see in GitLab (admin token)</option>
			</select>
			{#if authType === 'none'}
				<div class="alert alert-warning perm-alert" role="status">
					Every Sourcegull user can search every repository of this connection, including private ones. To apply GitLab
					permissions, set up <a href={configSectionURL('auth')}>sign-in with GitLab</a> and choose the second option.
				</div>
			{:else if authType === 'oauth'}
				<span class="hint">
					Needs a GitLab sign-in provider for {host} in the
					<a href={configSectionURL('auth')}>site configuration</a>. Users who never signed in with GitLab see only
					public repositories.
				</span>
			{:else}
				<span class="hint">
					Sourcegull users are matched to GitLab users with the same username. The token must belong to a GitLab
					administrator with the <code class="code-inline">sudo</code> scope (self-managed GitLab).
				</span>
			{/if}
		</div>
	</section>

	<details class="sub advanced">
		<summary>Advanced</summary>
		<div class="field">
			<label for="gl-git-url">Clone over</label>
			<select
				id="gl-git-url"
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
			<label for="gl-rate">Requests per hour to GitLab</label>
			<input
				id="gl-rate"
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
			<span class="hint">Shared by every Sourcegull process. Empty: 36,000.</span>
		</div>
		<div class="field">
			<label for="gl-path-pattern">Repository name pattern</label>
			<input
				id="gl-path-pattern"
				class="input mono"
				placeholder={'{host}/{pathWithNamespace}'}
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
		<label class="checkbox">
			<input
				type="checkbox"
				checked={doc.markInternalReposAsPublic === true}
				{disabled}
				onchange={(e) => onedit([[['markInternalReposAsPublic'], checked(e) || undefined]])}
			/>
			Treat internal projects as public (visible to every Sourcegull user)
		</label>
		<label class="checkbox">
			<input
				type="checkbox"
				checked={doc['webhooks.autoRegister'] === true}
				{disabled}
				onchange={(e) => onedit([[['webhooks.autoRegister'], checked(e) || undefined]])}
			/>
			Register webhooks on every project automatically (needs the <code class="code-inline">api</code> scope and the Maintainer
			role)
		</label>
		<div class="field cert">
			<label for="gl-cert">TLS certificate (PEM)</label>
			<textarea
				id="gl-cert"
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
			<span class="hint">Only for a self-managed GitLab with a self-signed or internal certificate authority.</span>
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
	.gitlab-fields {
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
	.advanced .checkbox,
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
	.checkbox.inline {
		display: inline-flex;
	}
	.hint.block {
		display: block;
	}
	.inline-input {
		display: inline-block;
		width: 22rem;
		max-width: 100%;
		margin: 0 var(--space-1);
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
		margin: var(--space-2) 0 0;
		font-size: var(--text-sm);
	}
	.json-only {
		margin: 0 0 var(--space-3);
	}
</style>
