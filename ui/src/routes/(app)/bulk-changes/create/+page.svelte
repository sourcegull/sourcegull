<!--
	Create a bulk change: a name and a namespace (the viewer or one of their
	organizations). `createEmptyBatchChange` makes a draft with an empty spec, and the spec
	editor opens.
-->
<script lang="ts">
	import { goto } from '$app/navigation';
	import { onMount } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		CREATE_EMPTY_BATCH_CHANGE,
		editURL,
		isValidBatchChangeName,
		loadBatchesAccess,
		type BatchesAccess,
		type CreateEmptyBatchChangeResult
	} from '#lib/batches/spec.ts';

	let access = $state.raw<BatchesAccess | null>(null);
	let loadError = $state<string | null>(null);
	let namespace = $state('');
	let name = $state('');
	let submitting = $state(false);
	let error = $state<string | null>(null);

	const namespaces = $derived(
		access?.available
			? [
					{ id: access.viewer.id, label: access.viewer.username },
					...access.viewer.organizations.nodes.map((o) => ({ id: o.id, label: o.displayName || o.name }))
				]
			: []
	);
	const nameInvalid = $derived(name !== '' && !isValidBatchChangeName(name));

	onMount(async () => {
		try {
			access = await loadBatchesAccess();
			if (access.available) namespace = access.viewer.id;
		} catch (err) {
			loadError = errorMessage(err);
		}
	});

	async function create(event: SubmitEvent): Promise<void> {
		event.preventDefault();
		if (!isValidBatchChangeName(name)) return;
		submitting = true;
		error = null;
		try {
			const r = await graphql<CreateEmptyBatchChangeResult, { namespace: string; name: string }>(
				CREATE_EMPTY_BATCH_CHANGE,
				{ namespace, name }
			);
			await goto(editURL(r.createEmptyBatchChange.url));
		} catch (err) {
			error = errorMessage(err);
		} finally {
			submitting = false;
		}
	}
</script>

<svelte:head><title>Create bulk change · Sourcegull</title></svelte:head>

<div class="page page-narrow">
	<nav class="crumbs" aria-label="Breadcrumbs"><a href="/bulk-changes">Bulk changes</a> / Create</nav>
	<div class="page-header">
		<div>
			<h1>Create bulk change</h1>
			<p class="page-description">
				A bulk change runs the steps of a bulk change spec in many repositories and opens a merge request for each change.
				Name it here, then write the spec and run it on the server.
			</p>
		</div>
	</div>

	{#if loadError}
		<div class="alert alert-danger" role="alert">{loadError}</div>
	{:else if access === null}
		<p class="loading"><span class="spinner"></span>Loading…</p>
	{:else if !access.available}
		<div class="alert alert-warning" role="alert">{access.message}</div>
	{:else if !access.canWrite}
		<div class="alert alert-warning" role="alert">
			You do not have permission to create bulk changes (<code>BATCH_CHANGES#WRITE</code>).
		</div>
	{:else}
		{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}
		<form class="card" onsubmit={create}>
			<div class="field">
				<label for="bc-namespace">Owner</label>
				<select id="bc-namespace" class="input" bind:value={namespace}>
					{#each namespaces as ns (ns.id)}
						<option value={ns.id}>{ns.label}</option>
					{/each}
				</select>
				<span class="hint">The user or organization the bulk change belongs to.</span>
			</div>
			<div class="field">
				<label for="bc-name">Bulk change name</label>
				<input
					id="bc-name"
					class="input"
					required
					autocomplete="off"
					placeholder="my-batch-change"
					aria-invalid={nameInvalid}
					aria-describedby="bc-name-hint"
					bind:value={name}
				/>
				<span id="bc-name-hint" class="hint" class:invalid={nameInvalid}>
					Letters, digits, underscores, dots and dashes only.
				</span>
			</div>
			<div class="toolbar">
				<button class="btn btn-primary" type="submit" disabled={submitting || name === '' || nameInvalid}>
					{submitting ? 'Creating…' : 'Create'}
				</button>
				<a class="btn" href="/bulk-changes">Cancel</a>
			</div>
		</form>
	{/if}
</div>

<style>
	.hint.invalid {
		color: var(--color-danger);
	}
	@media (max-width: 40rem) {
		.card {
			padding: var(--space-4);
		}
	}
</style>
