<!--
	Add a code host connection: GitLab, GitHub (`?kind=github`) or Gitea (`?kind=gitea`). When the
	resources file owns every connection, the page says so instead of offering the form.
-->
<script lang="ts">
	import { beforeNavigate, goto } from '$app/navigation';
	import { page } from '$app/state';
	import { onMount } from 'svelte';
	import CodeHostLogo from '#lib/components/CodeHostLogo.svelte';
	import ManagedNotice from '#lib/components/ManagedNotice.svelte';
	import ConnectionForm from '#lib/connections/ConnectionForm.svelte';
	import {
		ADD_EXTERNAL_SERVICE,
		KINDS,
		defaultConfig,
		kindLabel,
		parseKind,
		setFlash,
		validationMessages,
		type AddExternalServiceResult
	} from '#lib/connections/queries.ts';
	import { DECLARED_KINDS, isExclusive, type DeclaredKindsResult } from '#lib/declared-config.ts';
	import { errorMessage, graphql } from '#lib/graphql.ts';

	const kind = $derived(parseKind(page.url.searchParams.get('kind')));
	const label = $derived(kindLabel(kind));

	let busy = $state(false);
	let error = $state<string | null>(null);
	let errors = $state<string[] | null>(null);
	let dirty = $state(false);
	let saved = false;
	let exclusive = $state(false);

	onMount(async () => {
		try {
			const r = await graphql<DeclaredKindsResult>(DECLARED_KINDS);
			exclusive = isExclusive(r.site.declaredConfigFiles, 'connections');
		} catch {
			// The form stays; a save the server refuses shows its error.
		}
	});

	async function create(displayName: string, config: string): Promise<void> {
		busy = true;
		error = null;
		errors = null;
		try {
			const r = await graphql<AddExternalServiceResult, { input: object }>(ADD_EXTERNAL_SERVICE, {
				input: { kind, displayName, config }
			});
			saved = true;
			setFlash(r.addExternalService.warning);
			await goto(`/site-admin/external-services/${encodeURIComponent(r.addExternalService.id)}`);
		} catch (err) {
			error = errorMessage(err);
			errors = validationMessages(err);
		} finally {
			busy = false;
		}
	}

	beforeNavigate(({ cancel, willUnload }) => {
		if (!dirty || saved) return;
		if (willUnload || !confirm('Discard this new connection?')) cancel();
	});
</script>

<svelte:head><title>Add code host connection · Site admin · Sourcegull</title></svelte:head>

<p class="crumbs"><a href="/site-admin/external-services">Code host connections</a> /</p>
<div class="page-header">
	<div>
		<h1>Add {label} connection</h1>
		<p class="page-description">
			{#if kind === 'GITHUB'}
				Connect github.com or a GitHub Enterprise Server: enter its URL and an access token, choose which
				repositories to mirror, and test the connection.
			{:else if kind === 'GITEA'}
				Connect a Gitea instance: enter its URL and an access token, choose which repositories to mirror, and
				test the connection.
			{:else}
				Connect gitlab.com or a self-managed GitLab: enter its URL and an access token, choose which projects to
				mirror, and test the connection.
			{/if}
			The first sync starts as soon as the connection is saved; the token is stored encrypted.
		</p>
	</div>
</div>

{#if exclusive}
	<ManagedNotice kind="connections" />
{:else}
	<nav class="kinds" aria-label="Code host">
		{#each KINDS as k (k)}
			<a
				class="kind"
				href="/site-admin/external-services/new?kind={k.toLowerCase()}"
				aria-current={k === kind ? 'page' : undefined}
				data-sveltekit-replacestate
			>
				<CodeHostLogo kind={k} />
				{kindLabel(k)}
			</a>
		{/each}
	</nav>

	{#key kind}
		<ConnectionForm
			{kind}
			initialName={label}
			initialConfig={defaultConfig(kind)}
			submitLabel="Add connection"
			{busy}
			{error}
			{errors}
			onsubmit={create}
			oncancel={() => goto('/site-admin/external-services')}
			ondirty={(d) => (dirty = d)}
		/>
	{/key}
{/if}

<style>
	.crumbs {
		margin: 0 0 var(--space-2);
		font-size: var(--text-sm);
		color: var(--color-text-muted);
	}
	.kinds {
		display: flex;
		flex-wrap: wrap;
		gap: var(--space-2);
		margin-bottom: var(--space-4);
	}
	.kind {
		display: inline-flex;
		align-items: center;
		gap: var(--space-2);
		padding: var(--space-2) var(--space-4);
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
		color: var(--color-text);
		text-decoration: none;
	}
	.kind:hover {
		background: var(--color-bg-subtle);
	}
	.kind[aria-current='page'] {
		border-color: var(--color-primary);
		box-shadow: 0 0 0 1px var(--color-primary);
		font-weight: var(--weight-medium);
	}
</style>
