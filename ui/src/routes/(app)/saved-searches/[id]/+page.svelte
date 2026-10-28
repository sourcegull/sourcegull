<script lang="ts">
	import Icon from '#lib/components/Icon.svelte';
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import SavedSearchForm, { type SavedSearchFields } from '#lib/search/SavedSearchForm.svelte';
	import {
		CHANGE_SAVED_SEARCH_VISIBILITY,
		DELETE_SAVED_SEARCH,
		SAVED_SEARCH,
		TRANSFER_SAVED_SEARCH,
		UPDATE_SAVED_SEARCH,
		VIEWER_NAMESPACES,
		namespaceLabel,
		type SavedSearchNode,
		type SavedSearchResult,
		type UpdateSavedSearchVariables,
		type ViewerNamespacesResult
	} from '#lib/search/queries.ts';
	import { searchQueryURL } from '#lib/search/query.ts';

	const id = $derived(page.params.id ?? '');

	let search = $state.raw<SavedSearchNode | null>(null);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let editing = $state(false);
	let busy = $state(false);
	let actionError = $state<string | null>(null);
	let namespaces = $state.raw<{ id: string; label: string }[]>([]);
	let newOwner = $state('');

	async function load(nodeId: string): Promise<void> {
		loading = true;
		error = null;
		try {
			const r = await graphql<SavedSearchResult, { id: string }>(SAVED_SEARCH, { id: nodeId });
			search = r.node && r.node.__typename === 'SavedSearch' ? (r.node as SavedSearchNode) : null;
			if (!search) error = 'Saved search not found.';
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loading = false;
		}
	}

	async function loadNamespaces(): Promise<void> {
		try {
			const r = await graphql<ViewerNamespacesResult>(VIEWER_NAMESPACES);
			const u = r.currentUser;
			if (!u) return;
			namespaces = [
				{ id: u.id, label: `@${u.username}` },
				...u.organizations.nodes.map((o) => ({ id: o.id, label: `@${o.name}` }))
			];
		} catch {
			namespaces = [];
		}
	}

	$effect(() => {
		void load(id);
	});

	$effect(() => {
		if (search?.viewerCanAdminister && namespaces.length === 0) void loadNamespaces();
	});

	async function act(f: () => Promise<unknown>, reload = true): Promise<void> {
		busy = true;
		actionError = null;
		try {
			await f();
			if (reload) await load(id);
		} catch (err) {
			actionError = errorMessage(err);
		} finally {
			busy = false;
		}
	}

	function save(v: SavedSearchFields): void {
		void act(async () => {
			await graphql<unknown, UpdateSavedSearchVariables>(UPDATE_SAVED_SEARCH, { id, input: v });
			editing = false;
		});
	}

	function toggleVisibility(): void {
		if (!search) return;
		const newVisibility = search.visibility === 'PUBLIC' ? 'SECRET' : 'PUBLIC';
		void act(() => graphql(CHANGE_SAVED_SEARCH_VISIBILITY, { id, newVisibility }));
	}

	function transfer(): void {
		if (!newOwner) return;
		void act(() => graphql(TRANSFER_SAVED_SEARCH, { id, newOwner }));
	}

	function remove(): void {
		if (!search || !confirm(`Delete the saved search “${search.description}”?`)) return;
		void act(async () => {
			await graphql(DELETE_SAVED_SEARCH, { id });
			await goto('/saved-searches');
		}, false);
	}
</script>

<svelte:head><title>{search?.description ?? 'Saved search'} · Sourcegull</title></svelte:head>

<div class="page narrow">
	<p class="crumbs"><a href="/saved-searches">Saved searches</a> /</p>
	{#if loading && !search}
		<p class="loading"><span class="spinner"></span>Loading…</p>
	{:else if error}
		<div class="alert alert-danger" role="alert">{error}</div>
	{:else if search}
		<div class="page-header">
			<div>
				<h1 class="title">
					{search.description}
					{#if search.draft}<span class="badge badge-warning">draft</span>{/if}
					<span class="badge" class:badge-primary={search.visibility === 'PUBLIC'}>{search.visibility.toLowerCase()}</span>
				</h1>
				<p class="page-description">
					Owned by {namespaceLabel(search.owner)}
					{#if search.createdBy}· created by @{search.createdBy.username}{/if}
					· updated {new Date(search.updatedAt).toLocaleString()}
				</p>
			</div>
			<div class="page-actions">
				{#if search.viewerCanAdminister && !editing}
					<button type="button" class="btn" disabled={busy} onclick={() => (editing = true)}>Edit</button>
				{/if}
				<a class="btn btn-primary" href={searchQueryURL(search.query)}><Icon name="search" size={16} />Run search</a>
			</div>
		</div>

		{#if editing}
			<section class="card">
				<SavedSearchForm
					initial={{ description: search.description, query: search.query, draft: search.draft }}
					submitLabel="Save changes"
					error={actionError}
					{busy}
					onsubmit={save}
					oncancel={() => (editing = false)}
				/>
			</section>
		{:else}
			<pre class="query card mono">{search.query}</pre>
		{/if}

		{#if search.viewerCanAdminister && !editing}
			{#if actionError}<div class="alert alert-danger" role="alert">{actionError}</div>{/if}
			<section class="card settings" aria-label="Sharing">
				<div class="row">
					<div>
						<h3>Visibility</h3>
						<p class="muted small">Public saved searches are visible to every user; publishing needs a site admin.</p>
					</div>
					<button type="button" class="btn" disabled={busy} onclick={toggleVisibility}>
						{search.visibility === 'PUBLIC' ? 'Make secret' : 'Make public'}
					</button>
				</div>
				{#if namespaces.length > 1}
					<div class="row">
						<div>
							<h3><label for="ss-owner">Transfer to</label></h3>
							<p class="muted small">Move it to another user or organization namespace.</p>
						</div>
						<div class="toolbar">
							<select id="ss-owner" class="input" bind:value={newOwner}>
								<option value="">Choose a namespace…</option>
								{#each namespaces.filter((n) => n.id !== search?.owner.id) as n (n.id)}
									<option value={n.id}>{n.label}</option>
								{/each}
							</select>
							<button type="button" class="btn" disabled={busy || !newOwner} onclick={transfer}>Transfer</button>
						</div>
					</div>
				{/if}
				<div class="row">
					<div>
						<h3>Delete</h3>
						<p class="muted small">Removes it for everyone who can see it.</p>
					</div>
					<button type="button" class="btn btn-danger" disabled={busy} onclick={remove}>Delete</button>
				</div>
			</section>
		{/if}
	{/if}
</div>

<style>
	.narrow {
		max-width: 60rem;
	}
	.title {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: var(--space-2);
	}
	.query {
		margin: 0 0 var(--space-6);
		padding: var(--space-4);
		white-space: pre-wrap;
		overflow-wrap: anywhere;
	}
	.settings {
		padding: 0;
	}
	.row {
		display: flex;
		align-items: center;
		justify-content: space-between;
		flex-wrap: wrap;
		gap: var(--space-3);
		padding: var(--space-4) var(--space-5);
		border-bottom: 1px solid var(--color-border);
	}
	.row:last-child {
		border-bottom: 0;
	}
	.row h3 {
		margin: 0;
		font-size: var(--text-md);
	}
	.row p {
		margin: var(--space-1) 0 0;
	}
	.row select {
		width: auto;
	}
</style>
