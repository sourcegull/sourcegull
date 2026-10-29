<script lang="ts">
	import { tick } from 'svelte';
	import ConfigField from './ConfigField.svelte';
	import CopyField from './CopyField.svelte';
	import {
		GITLAB_SCOPES,
		PROVIDER_TYPES,
		giteaApplicationURLs,
		giteaBase,
		githubBase,
		githubOAuthAppURLs,
		gitlabApplicationURLs,
		gitlabBase,
		isHttpURL,
		missingRequired,
		newProvider,
		providerSummary,
		isSupportedProviderType,
		providerTypeInfo,
		providerVariant,
		redirectURI,
		type ProviderType
	} from '#lib/auth-providers.ts';
	import {
		buildField,
		getAt,
		issuesAt,
		jsonEqual,
		type ConfigIssue,
		type ConfigPath,
		type FormField,
		type JsonSchema
	} from '#lib/site-config-form.ts';

	interface Props {
		schema: JsonSchema;
		doc: Record<string, unknown>;
		saved: Record<string, unknown> | null;
		issues: ConfigIssue[];
		disabled?: boolean;
		onchange: (path: ConfigPath, value: unknown) => void;
	}

	let { schema, doc, saved, issues, disabled = false, onchange }: Props = $props();

	const KEY = 'auth.providers';
	/** Field order per provider type (the schema lists properties alphabetically). */
	const ORDER = [
		'url',
		'issuer',
		'clientID',
		'clientSecret',
		'usernameHeader',
		'emailHeader',
		'stripUsernameHeaderPrefix',
		'trustedProxies',
		'displayName',
		'configID',
		'allowSignup',
		'linkByEmail',
		'requireEmailDomain',
		'allowGroups',
		'allowOrgs',
		'groupsClaim'
	];

	const inDocument = $derived(Array.isArray(doc[KEY]));
	const defaultProviders = $derived(
		(Array.isArray(schema.properties?.[KEY]?.default) ? schema.properties?.[KEY]?.default : [{ type: 'builtin' }]) as Record<
			string,
			unknown
		>[]
	);
	const providers = $derived(
		(inDocument ? (doc[KEY] as unknown[]) : defaultProviders).map((p) =>
			p && typeof p === 'object' ? (p as Record<string, unknown>) : {}
		)
	);
	const savedProviders = $derived(saved && Array.isArray(saved[KEY]) ? (saved[KEY] as unknown[]) : null);
	const externalURL = $derived(doc.externalURL);
	const externalURLField = $derived(buildField(schema, 'externalURL', schema.properties?.externalURL ?? {}, ['externalURL']));
	const listIssues = $derived(issuesAt(issues, [KEY], false));

	let open = $state<number | null>(null);
	let adding = $state(false);

	/** Applies a change below `auth.providers`; writes the whole list first when it is not in the document. */
	function change(path: ConfigPath, value: unknown): void {
		if (inDocument) return onchange(path, value);
		const list = structuredClone($state.snapshot(providers)) as unknown[];
		const rel = path.slice(1);
		if (rel.length === 0) return onchange([KEY], value);
		if (rel.length === 1 && rel[0] === -1) list.push(value);
		else if (rel.length === 1 && value === undefined) list.splice(Number(rel[0]), 1);
		else {
			let cur = list as unknown as Record<string, unknown>;
			for (const seg of rel.slice(0, -1)) cur = cur[seg as string] as Record<string, unknown>;
			const last = rel[rel.length - 1] as string;
			if (value === undefined) delete cur[last];
			else cur[last] = value;
		}
		onchange([KEY], list);
	}

	let list: HTMLUListElement;

	/** Appends a provider and opens its setup. */
	async function add(type: ProviderType): Promise<void> {
		const index = providers.length;
		change([KEY, -1], newProvider(type));
		open = index;
		adding = false;
		await tick();
		list.children[index]?.scrollIntoView({ block: 'nearest', behavior: 'smooth' });
	}

	function remove(i: number): void {
		const label = providerTypeInfo(providers[i]?.type).label;
		if (!confirm(`Remove the ${label} sign-in provider? Users who sign in with it can no longer sign in.`)) return;
		change([KEY, i], undefined);
		if (open === i) open = null;
		else if (open !== null && open > i) open--;
	}

	function fieldsOf(p: Record<string, unknown>, i: number): FormField[] {
		const variant = providerVariant(schema, p.type);
		if (!variant?.properties) return [];
		const props = variant.properties;
		const required = new Set(variant.required ?? []);
		const keys = Object.keys(props)
			.filter((k) => k !== 'type')
			.sort((a, b) => rank(a) - rank(b) || a.localeCompare(b));
		return keys.map((k) => buildField(schema, k, props[k] ?? {}, [KEY, i, k], { required: required.has(k) }));
	}
	function rank(k: string): number {
		const i = ORDER.indexOf(k);
		return i < 0 ? ORDER.length : i;
	}

	const hasBuiltin = $derived(providers.some((p) => p.type === 'builtin'));
	const hasSso = $derived(providers.some((p) => p.type !== 'builtin'));
</script>

<section class="providers" aria-labelledby="providers-title">
	<div class="section-header">
		<h3 id="providers-title">Sign-in providers</h3>
		<code class="key">{KEY}</code>
	</div>
	<p class="hint">
		How users sign in. Several providers can be enabled at once; each gets a button on the sign-in page.
		{#if !inDocument}<strong>Not set:</strong> the default applies (username and password only). Changing it writes the
			list to the configuration.{/if}
	</p>
	{#each listIssues as issue, i (i)}<p class="error-text">{issue.message}</p>{/each}

	{#if !hasBuiltin && hasSso}
		<div class="alert alert-info">
			Username and password sign-in is off. Make sure at least one site admin can sign in with the providers below
			before you save.
		</div>
	{/if}

	<ul class="provider-list" bind:this={list}>
		{#each providers as provider, i (i)}
			{@const info = providerTypeInfo(provider.type)}
			{@const variant = providerVariant(schema, provider.type)}
			{@const missing = missingRequired(variant, provider)}
			{@const isOpen = open === i}
			{@const changed = savedProviders !== null ? !jsonEqual(provider, savedProviders[i]) : inDocument}
			{@const providerIssues = issuesAt(issues, [KEY, i], true)}
			<li class="provider card" class:open={isOpen}>
				<div class="provider-head">
					<div class="provider-title">
						<span class="provider-name">
							{typeof provider.displayName === 'string' && provider.displayName ? provider.displayName : info.label}
						</span>
						<span class="badge">{info.label}</span>
						{#if missing.length > 0}<span class="badge badge-warning">Incomplete</span>{/if}
						{#if providerIssues.length > 0}<span class="badge badge-danger">{providerIssues.length} problem{providerIssues.length === 1 ? '' : 's'}</span>{/if}
						{#if changed}<span class="badge badge-warning">Changed</span>{/if}
						{#if !isSupportedProviderType(provider.type)}<span class="badge badge-danger">Not supported</span>{/if}
					</div>
					<p class="provider-meta">{providerSummary(provider)}</p>
					<div class="provider-actions">
						<button
							class="btn btn-sm"
							type="button"
							aria-expanded={isOpen}
							aria-controls="provider-{i}"
							onclick={() => (open = isOpen ? null : i)}
						>
							{isOpen ? 'Close' : missing.length > 0 ? 'Set up' : 'Edit'}
						</button>
						<button
							class="btn btn-sm btn-danger"
							type="button"
							onclick={() => remove(i)}
							disabled={disabled || providers.length === 1}
							title={providers.length === 1 ? 'At least one provider is needed to sign in' : undefined}
						>
							Remove
						</button>
					</div>
				</div>

				{#if isOpen}
					<div class="provider-body" id="provider-{i}">
						{#if provider.type === 'gitlab'}
							{@const apps = gitlabApplicationURLs(provider.url)}
							{@const redirect = redirectURI(externalURL, 'gitlab')}
							<details class="setup" open={missing.length > 0}>
								<summary>How to set up sign-in with GitLab</summary>
								<ol class="steps">
									<li>
										<strong>Set the external URL of Sourcegull.</strong>
										GitLab sends users back to it after they sign in, so it must be the URL people use to reach this
										instance.
										{#if isHttpURL(externalURL)}
											Currently <code class="code-inline">{externalURL}</code>.
										{:else}
											<div class="alert alert-warning step-alert" role="status">
												<code>externalURL</code> is not set{externalURL ? ' to a valid http(s) URL' : ''}. Set it here
												first; the redirect URI below depends on it.
											</div>
											<div class="inline-field">
												<ConfigField field={externalURLField} {doc} {saved} {issues} {disabled} {onchange} />
											</div>
										{/if}
									</li>
									<li>
										<strong>Create an OAuth application in GitLab</strong> at <code class="code-inline">{gitlabBase(provider.url)}</code>
										(change <em>GitLab URL</em> below for a self-managed instance). Any of these works:
										<ul>
											<li>
												<a href={apps.admin} target="_blank" rel="noopener noreferrer">Admin area › Applications</a>: an
												instance-wide application (self-managed GitLab, administrators only). Recommended: it doesn't depend
												on one person's account.
											</li>
											<li>
												A group's <em>Settings › Applications</em> (<code class="code-inline">{apps.group}</code>): owned by
												a group you maintain.
											</li>
											<li>
												<a href={apps.user} target="_blank" rel="noopener noreferrer">User settings › Applications</a>:
												owned by your own account (the only option on gitlab.com without a group).
											</li>
										</ul>
									</li>
									<li>
										<strong>Name</strong> it e.g. “Sourcegull”, and set the <strong>Redirect URI</strong> to exactly:
										{#if redirect}
											<CopyField value={redirect} label="Redirect URI" />
										{:else}
											<p><code class="code-inline">&lt;externalURL&gt;/.auth/gitlab/callback</code> <span class="muted">(set the external URL first)</span></p>
										{/if}
									</li>
									<li><strong>Confidential:</strong> keep it checked. Sourcegull keeps the secret on the server.</li>
									<li>
										<strong>Scopes:</strong> select only these two:
										<ul>
											{#each GITLAB_SCOPES as s (s.scope)}
												<li><code class="code-inline">{s.scope}</code>: {s.purpose}</li>
											{/each}
										</ul>
										Sourcegull never writes to GitLab through sign-in tokens, so no <code class="code-inline">api</code> or
										write scope is needed.
									</li>
									<li>
										<strong>Save the application</strong>, then copy its <em>Application ID</em> into
										<em>Client ID</em> and its <em>Secret</em> into <em>Client secret</em> below. GitLab shows the secret
										only once.
									</li>
									<li>
										<strong>Optional:</strong> <em>Display name</em> labels the sign-in button; <em>Allow signup</em>
										creates a Sourcegull account on first sign-in (without it, GitLab users can only sign in to an existing
										account with the same verified email); <em>Allow groups</em> limits sign-in to members of those groups
										(full paths, e.g. <code class="code-inline">acme/platform</code>).
									</li>
									<li>
										<strong>Apply GitLab repository permissions</strong> to users who sign in this way: on the GitLab
										<a href="/site-admin/external-services">code host connection</a>, add
										<code class="code-inline">"authorization": {'{'} "identityProvider": {'{'} "type": "oauth" {'}'} {'}'}</code>.
										The connection's <code class="code-inline">url</code> must be the same GitLab instance.
									</li>
									<li><strong>Save</strong> this page and sign in from a private window to try it.</li>
								</ol>
							</details>
						{:else if provider.type === 'github'}
							{@const apps = githubOAuthAppURLs(provider.url)}
							{@const redirect = redirectURI(externalURL, 'github')}
							<details class="setup" open={missing.length > 0}>
								<summary>How to set up sign-in with GitHub</summary>
								<ol class="steps">
									<li>
										<strong>Set the external URL</strong>
										{#if isHttpURL(externalURL)}(currently <code class="code-inline">{externalURL}</code>).{:else}first:
											<div class="inline-field">
												<ConfigField field={externalURLField} {doc} {saved} {issues} {disabled} {onchange} />
											</div>
										{/if}
									</li>
									<li>
										<strong>Register an OAuth app in GitHub</strong> at <code class="code-inline">{githubBase(provider.url)}</code>
										(change <em>Url</em> below for GitHub Enterprise Server), either
										<a href={apps.user} target="_blank" rel="noopener noreferrer">for your account</a> or for an
										organization (<code class="code-inline">{apps.org}</code>), recommended so it doesn't depend on one
										person's account. Use an OAuth app, not a GitHub App.
									</li>
									<li>
										Set the <strong>Authorization callback URL</strong> to exactly:
										{#if redirect}
											<CopyField value={redirect} label="Callback URL" />
										{:else}
											<p><code class="code-inline">&lt;externalURL&gt;/.auth/github/callback</code> <span class="muted">(set the external URL first)</span></p>
										{/if}
									</li>
									<li>
										<strong>Register the application</strong>, generate a client secret, and copy the <em>Client ID</em> and
										the secret below. GitHub shows the secret only once.
									</li>
									<li>
										Sourcegull asks for the scopes <code class="code-inline">read:user</code> and
										<code class="code-inline">user:email</code> (to create or match the account by verified email), and
										<code class="code-inline">read:org</code> only when <em>Allow orgs</em> is set.
									</li>
									<li>
										<strong>Optional:</strong> <em>Allow signup</em> creates a Sourcegull account on first sign-in (without it,
										GitHub users can only sign in to an existing account with the same verified email); <em>Allow orgs</em>
										limits sign-in to members of those organizations. An organization that restricts OAuth app access must
										approve the app for its memberships to be visible.
									</li>
									<li><strong>Save</strong> this page and sign in from a private window to try it.</li>
								</ol>
							</details>
						{:else if provider.type === 'gitea'}
							{@const apps = giteaApplicationURLs(provider.url)}
							{@const redirect = redirectURI(externalURL, 'gitea')}
							<details class="setup" open={missing.length > 0}>
								<summary>How to set up sign-in with Gitea</summary>
								<ol class="steps">
									<li>
										<strong>Set the external URL</strong>
										{#if isHttpURL(externalURL)}(currently <code class="code-inline">{externalURL}</code>).{:else}first:
											<div class="inline-field">
												<ConfigField field={externalURLField} {doc} {saved} {issues} {disabled} {onchange} />
											</div>
										{/if}
									</li>
									<li>
										<strong>Set <em>Url</em> below</strong> to the Gitea instance{#if giteaBase(provider.url)}
											(currently <code class="code-inline">{giteaBase(provider.url)}</code>){/if}.
									</li>
									<li>
										<strong>Create an OAuth2 application in Gitea</strong>:
										{#if apps}
											<a href={apps.admin} target="_blank" rel="noopener noreferrer">Site Administration › Applications</a>
											(instance-wide, administrators only; recommended) or
											<a href={apps.user} target="_blank" rel="noopener noreferrer">Settings › Applications</a> (your own
											account).
										{:else}
											under Site Administration › Applications (instance-wide) or your Settings › Applications.
										{/if}
										Keep <strong>Confidential client</strong> checked, and set the <strong>Redirect URI</strong> to exactly:
										{#if redirect}
											<CopyField value={redirect} label="Redirect URI" />
										{:else}
											<p><code class="code-inline">&lt;externalURL&gt;/.auth/gitea/callback</code> <span class="muted">(set the external URL first)</span></p>
										{/if}
									</li>
									<li>
										Copy the <em>Client ID</em> and <em>Client Secret</em> below. Gitea shows the secret only once. Sourcegull
										asks for the scopes <code class="code-inline">read:user</code> and
										<code class="code-inline">read:organization</code>.
									</li>
									<li>
										<strong>Optional:</strong> <em>Allow signup</em> creates a Sourcegull account on first sign-in (without it,
										Gitea users can only sign in to an existing account with the same verified email); <em>Allow orgs</em>
										limits sign-in to members of those organizations.
									</li>
									<li><strong>Save</strong> this page and sign in from a private window to try it.</li>
								</ol>
							</details>
						{:else if provider.type === 'openidconnect'}
							{@const redirect = redirectURI(externalURL, 'openidconnect')}
							<details class="setup" open={missing.length > 0}>
								<summary>How to set up OpenID Connect</summary>
								<ol class="steps">
									<li>
										<strong>Set the external URL</strong>
										{#if isHttpURL(externalURL)}(currently <code class="code-inline">{externalURL}</code>).{:else}first:
											<div class="inline-field">
												<ConfigField field={externalURLField} {doc} {saved} {issues} {disabled} {onchange} />
											</div>
										{/if}
									</li>
									<li>
										<strong>Register a confidential web application</strong> (client) at your identity provider with the
										redirect URI:
										{#if redirect}
											<CopyField value={redirect} label="Redirect URI" />
										{:else}
											<code class="code-inline">&lt;externalURL&gt;/.auth/openidconnect/callback</code>
										{/if}
										Sourcegull requests the scopes <code class="code-inline">openid email profile</code>.
									</li>
									<li>
										<strong>Issuer</strong> is the URL that serves <code class="code-inline">/.well-known/openid-configuration</code>
										(e.g. <code class="code-inline">https://keycloak.example.com/realms/acme</code>). Paste the client ID and
										secret below.
									</li>
								</ol>
							</details>
						{:else if provider.type === 'http-header'}
							<p class="alert alert-info">
								Only use this behind a reverse proxy that authenticates users and sets the header itself. List the
								proxy's networks in <em>Trusted proxies</em>; an empty list trusts no one, so nobody can sign in.
							</p>
						{:else if !isSupportedProviderType(provider.type)}
							<p class="alert alert-danger">
								{info.label} sign-in is not supported, and this provider is ignored. Remove it to save the configuration{provider.type ===
								'saml'
									? '; most SAML identity providers also offer OpenID Connect'
									: ''}.
							</p>
						{/if}

						<div class="provider-fields">
							{#each fieldsOf(provider, i) as field (field.key)}
								<ConfigField {field} {doc} {saved} {issues} {disabled} onchange={change} />
							{/each}
						</div>
					</div>
				{/if}
			</li>
		{/each}
	</ul>

	{#if adding}
		<div class="card add-menu" role="group" aria-label="Choose a provider type">
			<p class="label">Add a sign-in provider</p>
			<ul>
				{#each PROVIDER_TYPES.filter((t) => t.addable) as t (t.type)}
					<li>
						<button class="add-option" type="button" onclick={() => add(t.type)} {disabled}>
							<span class="label">{t.label}</span>
							<span class="muted small">{t.blurb}</span>
						</button>
					</li>
				{/each}
			</ul>
			<button class="btn btn-sm" type="button" onclick={() => (adding = false)}>Cancel</button>
		</div>
	{:else}
		<button class="btn" type="button" onclick={() => (adding = true)} {disabled}>+ Add provider</button>
	{/if}
</section>

<style>
	.providers {
		padding: var(--space-3) 0 var(--space-4);
	}
	.section-header {
		display: flex;
		align-items: baseline;
		gap: var(--space-2);
		flex-wrap: wrap;
	}
	.section-header h3 {
		margin: 0;
		font-size: var(--text-md);
	}
	.key {
		font-size: var(--text-xs);
		color: var(--color-text-subtle);
	}
	.hint {
		color: var(--color-text-muted);
		font-size: var(--text-sm);
		margin: var(--space-1) 0 var(--space-3);
		max-width: 48rem;
	}
	.provider-list {
		list-style: none;
		margin: 0 0 var(--space-3);
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
	}
	.provider {
		padding: var(--space-3) var(--space-4);
	}
	.provider.open {
		border-color: var(--color-border-strong);
	}
	.provider-head {
		display: grid;
		grid-template-columns: minmax(0, 1fr) auto;
		gap: var(--space-1) var(--space-3);
		align-items: center;
	}
	.provider-title {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: var(--space-2);
	}
	.provider-name {
		font-weight: var(--weight-semibold);
	}
	.provider-meta {
		grid-column: 1;
		margin: 0;
		color: var(--color-text-muted);
		font-size: var(--text-sm);
		overflow-wrap: anywhere;
	}
	.provider-actions {
		grid-column: 2;
		grid-row: 1 / span 2;
		display: flex;
		gap: var(--space-2);
	}
	@media (max-width: 600px) {
		.provider-head {
			grid-template-columns: minmax(0, 1fr);
		}
		.provider-actions {
			grid-column: 1;
			grid-row: auto;
		}
	}
	.provider-body {
		margin-top: var(--space-3);
		padding-top: var(--space-3);
		border-top: 1px solid var(--color-border);
	}
	.setup {
		background: var(--color-bg-subtle);
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
		padding: var(--space-2) var(--space-4);
		margin-bottom: var(--space-3);
		font-size: var(--text-sm);
	}
	.setup summary {
		cursor: pointer;
		font-weight: var(--weight-medium);
		padding: var(--space-1) 0;
	}
	.steps {
		margin: var(--space-2) 0 var(--space-2);
		padding-left: var(--space-5);
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
		max-width: 52rem;
	}
	.steps ul {
		margin: var(--space-1) 0;
		padding-left: var(--space-4);
	}
	.steps li {
		overflow-wrap: anywhere;
	}
	.step-alert {
		margin: var(--space-2) 0;
	}
	.inline-field {
		background: var(--color-surface);
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
		padding: 0 var(--space-3);
		margin-top: var(--space-2);
	}
	.add-menu {
		padding: var(--space-3) var(--space-4);
		max-width: 36rem;
	}
	.add-menu ul {
		list-style: none;
		margin: var(--space-2) 0 var(--space-3);
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: var(--space-1);
	}
	.add-option {
		display: flex;
		flex-direction: column;
		align-items: flex-start;
		width: 100%;
		text-align: left;
		padding: var(--space-2) var(--space-3);
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
		background: var(--color-surface);
		color: inherit;
		font: inherit;
		cursor: pointer;
	}
	.add-option:hover {
		background: var(--color-bg-hover);
		border-color: var(--color-border-strong);
	}
</style>
