<!--
	Bulk changes credentials: the code hosts changesets can be published
	to and the token used for each. Site admins manage the site's credentials (used when a user
	has none); users manage their own, falling back to the site's. Tokens are checked with the
	code host when added and never shown again.
-->
<script lang="ts">
	import RelatedLinks from '#lib/components/RelatedLinks.svelte';
	import { onMount } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import { formatDate } from './batch-change.ts';
	import {
		CREATE_CREDENTIAL,
		DELETE_CREDENTIAL,
		SITE_CODE_HOSTS,
		USER_CODE_HOSTS,
		codeHostName,
		credentialStatus,
		tokenPageURL,
		type BatchChangesCodeHost,
		type SiteCodeHostsResult,
		type UserCodeHostsResult
	} from './credentials.ts';

	let { scope }: { scope: 'site' | 'user' } = $props();

	let hosts = $state.raw<BatchChangesCodeHost[]>([]);
	let userId = $state<string | null>(null);
	let loading = $state(true);
	let error = $state<string | null>(null);

	/** The code host URL a token is being added for. */
	let adding = $state<string | null>(null);
	let token = $state('');
	let username = $state('');
	let submitting = $state(false);
	let formError = $state<string | null>(null);

	async function load(): Promise<void> {
		loading = true;
		error = null;
		try {
			if (scope === 'site') {
				const r = await graphql<SiteCodeHostsResult, { first: number }>(SITE_CODE_HOSTS, { first: 100 });
				hosts = r.batchChangesCodeHosts.nodes;
			} else {
				const r = await graphql<UserCodeHostsResult, { first: number }>(USER_CODE_HOSTS, { first: 100 });
				userId = r.currentUser?.id ?? null;
				hosts = r.currentUser?.batchChangesCodeHosts.nodes ?? [];
			}
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loading = false;
		}
	}

	async function add(event: SubmitEvent, host: BatchChangesCodeHost): Promise<void> {
		event.preventDefault();
		submitting = true;
		formError = null;
		try {
			await graphql(CREATE_CREDENTIAL, {
				user: scope === 'user' ? userId : null,
				externalServiceKind: host.externalServiceKind,
				externalServiceURL: host.externalServiceURL,
				username: host.requiresUsername ? username.trim() || null : null,
				credential: token
			});
			adding = null;
			token = '';
			username = '';
			await load();
		} catch (err) {
			formError = errorMessage(err);
		} finally {
			submitting = false;
		}
	}

	async function remove(host: BatchChangesCodeHost): Promise<void> {
		const c = host.credential;
		if (!c) return;
		if (!confirm(`Remove the credential for ${codeHostName(host.externalServiceURL)}?`)) return;
		error = null;
		try {
			await graphql(DELETE_CREDENTIAL, { id: c.id });
			await load();
		} catch (err) {
			error = errorMessage(err);
		}
	}

	function startAdding(host: BatchChangesCodeHost): void {
		adding = host.externalServiceURL;
		token = '';
		username = '';
		formError = null;
	}

	onMount(() => {
		void load();
	});
</script>

<div class="page-header">
	<div>
		<h1>Bulk changes</h1>
		<RelatedLinks />
	</div>
</div>

<h2 class="section">Code host tokens</h2>
<p class="muted">
	{#if scope === 'site'}
		Changesets are published with the token of the user who applied the bulk change; users without
		one fall back to the site's token for that code host. Tokens need the <code>api</code> scope and
		write access to the repositories.
	{:else}
		Your tokens publish your bulk changes' changesets, so merge requests are opened as you. Without
		one, the site's token is used if a site admin added it. Tokens need the <code>api</code> scope.
	{/if}
</p>

{#if error}
	<div class="alert alert-danger" role="alert">{error}</div>
{/if}

<ul class="hosts" aria-busy={loading} aria-label="Code hosts">
	{#each hosts as host (host.externalServiceURL)}
		{@const status = credentialStatus(host, scope)}
		<li class="host card" data-code-host={host.externalServiceURL}>
			<div class="row">
				<div class="name">
					<strong>{codeHostName(host.externalServiceURL)}</strong>
					<span class="muted small">GitLab · {host.externalServiceURL}</span>
					{#if !host.hasWebhooks}
						<span class="muted small">No webhooks: changesets update on periodic syncs.</span>
					{/if}
				</div>
				<div class="status">
					{#if status === 'own'}
						<span class="badge badge-primary">Token added</span>
						<span class="muted small">{host.credential ? formatDate(host.credential.createdAt) : ''}</span>
					{:else if status === 'site'}
						<span class="badge">Using the site's token</span>
					{:else}
						<span class="badge">No token</span>
					{/if}
				</div>
				<div class="actions">
					{#if status === 'own'}
						<button class="btn btn-sm btn-danger" type="button" onclick={() => remove(host)}>Remove</button>
					{:else if adding !== host.externalServiceURL}
						<button class="btn btn-sm btn-primary" type="button" onclick={() => startAdding(host)}>Add token</button>
					{/if}
				</div>
			</div>
			{#if status === 'own' && host.credential?.sshPublicKey}
				<div class="field">
					<span class="label">SSH public key</span>
					<code class="ssh">{host.credential.sshPublicKey}</code>
				</div>
			{/if}
			{#if adding === host.externalServiceURL}
				<form class="add" onsubmit={(e) => add(e, host)} aria-label="Add token">
					<p class="muted small">
						Create a personal access token with the <code>api</code> scope on
						<a href={tokenPageURL(host.externalServiceURL)} rel="noopener noreferrer">{codeHostName(host.externalServiceURL)}</a>.
						It is checked with the code host before it is saved.
					</p>
					{#if host.requiresUsername}
						<div class="field">
							<label for="credential-username">Username</label>
							<input id="credential-username" class="input" bind:value={username} required />
						</div>
					{/if}
					<div class="field">
						<label for="credential-token">Personal access token</label>
						<!-- svelte-ignore a11y_autofocus -->
						<input
							id="credential-token"
							class="input mono"
							type="password"
							autocomplete="off"
							bind:value={token}
							required
							autofocus
						/>
					</div>
					{#if formError}
						<div class="alert alert-danger" role="alert">{formError}</div>
					{/if}
					<div class="toolbar">
						<button class="btn btn-primary" type="submit" disabled={submitting || !token}>
							{submitting ? 'Checking…' : 'Add token'}
						</button>
						<button class="btn" type="button" onclick={() => (adding = null)}>Cancel</button>
					</div>
				</form>
			{/if}
		</li>
	{:else}
		{#if !loading}
			<li class="empty muted">
				No code host connections. {scope === 'site' ? 'Add a GitLab connection first.' : 'Ask a site admin to connect GitLab.'}
			</li>
		{/if}
	{/each}
</ul>

<style>
	.section {
		font-size: 1.1rem;
		margin: 0 0 var(--space-2);
	}
	.hosts {
		list-style: none;
		margin: var(--space-4) 0 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
	}
	.host {
		padding: var(--space-3) var(--space-4);
	}
	.row {
		display: flex;
		align-items: center;
		gap: var(--space-4);
		flex-wrap: wrap;
	}
	.name {
		flex: 1 1 16rem;
		display: flex;
		flex-direction: column;
		min-width: 0;
		overflow-wrap: anywhere;
	}
	.status {
		display: flex;
		flex-direction: column;
		align-items: flex-start;
		gap: var(--space-1);
	}
	.add {
		margin-top: var(--space-3);
		max-width: 32rem;
	}
	.add p {
		margin: 0 0 var(--space-2);
	}
	.ssh {
		overflow-wrap: anywhere;
		font-size: 0.8em;
	}
	.empty {
		padding: var(--space-6) 0;
		text-align: center;
	}
	.small {
		font-size: 0.875em;
	}
</style>
