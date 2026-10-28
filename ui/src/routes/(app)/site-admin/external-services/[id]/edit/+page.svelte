<!--
	Edit a code host connection's name and configuration. A connection the resources file owns is
	shown read-only.
-->
<script lang="ts">
	import { beforeNavigate, goto } from '$app/navigation';
	import { page } from '$app/state';
	import JsoncEditor from '#lib/components/JsoncEditor.svelte';
	import ManagedNotice from '#lib/components/ManagedNotice.svelte';
	import ConnectionForm from '#lib/connections/ConnectionForm.svelte';
	import {
		EXTERNAL_SERVICE,
		UPDATE_EXTERNAL_SERVICE,
		setFlash,
		validationMessages,
		type ExternalServiceDetail,
		type ExternalServiceResult,
		type UpdateExternalServiceResult
	} from '#lib/connections/queries.ts';
	import { errorMessage, graphql } from '#lib/graphql.ts';

	const id = $derived(page.params.id ?? '');
	const detailURL = $derived(`/site-admin/external-services/${encodeURIComponent(id)}`);

	let service = $state.raw<ExternalServiceDetail | null>(null);
	let loadError = $state<string | null>(null);
	let busy = $state(false);
	let error = $state<string | null>(null);
	let errors = $state<string[] | null>(null);
	let dirty = $state(false);
	let saved = false;

	async function load(nodeId: string): Promise<void> {
		loadError = null;
		try {
			const r = await graphql<ExternalServiceResult, { id: string; first: number }>(EXTERNAL_SERVICE, {
				id: nodeId,
				first: 0
			});
			service = r.node?.__typename === 'ExternalService' ? (r.node as ExternalServiceDetail) : null;
			if (!service) loadError = 'Code host connection not found.';
		} catch (err) {
			loadError = errorMessage(err);
		}
	}

	$effect(() => {
		void load(id);
	});

	async function save(displayName: string, config: string): Promise<void> {
		if (!service || service.managedBy) return;
		busy = true;
		error = null;
		errors = null;
		try {
			const r = await graphql<UpdateExternalServiceResult, { input: object }>(UPDATE_EXTERNAL_SERVICE, {
				input: {
					id,
					displayName: displayName !== service.displayName ? displayName : null,
					config: config !== service.config ? config : null
				}
			});
			saved = true;
			setFlash(r.updateExternalService.warning ?? 'Saved.');
			await goto(detailURL);
		} catch (err) {
			error = errorMessage(err);
			errors = validationMessages(err);
		} finally {
			busy = false;
		}
	}

	beforeNavigate(({ cancel, willUnload }) => {
		if (!dirty || saved) return;
		if (willUnload || !confirm('Discard unsaved changes to this connection?')) cancel();
	});
</script>

<svelte:head><title>Edit {service?.displayName ?? 'connection'} · Site admin · Sourcegull</title></svelte:head>

<p class="crumbs">
	<a href="/site-admin/external-services">Code host connections</a> /
	{#if service}<a href={detailURL}>{service.displayName}</a> /{/if}
</p>
<div class="page-header">
	<div>
		<h1>Edit connection</h1>
		<p class="page-description">
			Changing the configuration queues a full sync. Repositories the new configuration no longer
			lists are removed after it.
		</p>
	</div>
</div>

{#if loadError}
	<div class="alert alert-danger" role="alert">{loadError}</div>
{:else if service?.managedBy}
	<ManagedNotice managedBy={service.managedBy} />
	<JsoncEditor value={service.config} readOnly label="Connection configuration (JSON with comments)" />
	<div class="toolbar actions">
		<a class="btn" href={detailURL}>Back to the connection</a>
	</div>
{:else if service}
	{#key service.id}
		<ConnectionForm
			kind={service.kind}
			serviceId={service.id}
			initialName={service.displayName}
			initialConfig={service.config}
			submitLabel="Save"
			{busy}
			{error}
			{errors}
			onsubmit={save}
			oncancel={() => goto(detailURL)}
			ondirty={(d) => (dirty = d)}
		/>
	{/key}
{:else}
	<p class="loading" aria-live="polite"><span class="spinner"></span> Loading…</p>
{/if}

<style>
	.actions {
		margin-top: var(--space-3);
	}
	.crumbs {
		margin: 0 0 var(--space-2);
		font-size: var(--text-sm);
		color: var(--color-text-muted);
	}
</style>
