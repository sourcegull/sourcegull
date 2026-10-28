<script lang="ts">
	import type { CreateSavedSearchMutation } from '#lib/graphql-types.ts';
	import { goto } from '$app/navigation';
	import Icon from '#lib/components/Icon.svelte';
	import { page } from '$app/state';
	import { onMount } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import SavedSearchForm, { type SavedSearchFields } from '#lib/search/SavedSearchForm.svelte';
	import {
		CREATE_SAVED_SEARCH,
		SAVED_SEARCHES,
		namespaceLabel,
		type CreateSavedSearchVariables,
		type SavedSearchNode,
		type SavedSearchesOrderBy,
		type SavedSearchesResult,
		type SavedSearchesVariables
	} from '#lib/search/queries.ts';
	import { searchQueryURL } from '#lib/search/query.ts';

	const PAGE_SIZE = 50;

	const user = $derived(page.data.auth?.user ?? null);

	let items = $state.raw<SavedSearchNode[]>([]);
	let totalCount = $state(0);
	let cursor = $state<string | null>(null);
	let hasMore = $state(false);
	let filter = $state('');
	let mineOnly = $state(false);
	let orderBy = $state<SavedSearchesOrderBy>('SAVED_SEARCH_UPDATED_AT');
	let loading = $state(true);
	let error = $state<string | null>(null);
	let seq = 0;

	// A `?new=<query>` link (from elsewhere) opens the form prefilled.
	const prefill = page.url.searchParams.get('new');
	let creating = $state(prefill !== null);
	let createError = $state<string | null>(null);
	let createBusy = $state(false);

	async function load(after: string | null): Promise<void> {
		const mine = ++seq;
		loading = true;
		error = null;
		try {
			const r = await graphql<SavedSearchesResult, SavedSearchesVariables>(SAVED_SEARCHES, {
				query: filter.trim() || null,
				owner: mineOnly && user ? user.id : null,
				first: PAGE_SIZE,
				after,
				orderBy
			});
			if (mine !== seq) return;
			items = after ? [...items, ...r.savedSearches.nodes] : r.savedSearches.nodes;
			totalCount = r.savedSearches.totalCount;
			cursor = r.savedSearches.pageInfo.endCursor;
			hasMore = r.savedSearches.pageInfo.hasNextPage;
		} catch (err) {
			if (mine === seq) error = errorMessage(err);
		} finally {
			if (mine === seq) loading = false;
		}
	}

	let debounce: ReturnType<typeof setTimeout> | undefined;
	function oninput(): void {
		clearTimeout(debounce);
		debounce = setTimeout(() => load(null), 200);
	}

	onMount(() => {
		void load(null);
		return () => clearTimeout(debounce);
	});

	async function create(v: SavedSearchFields): Promise<void> {
		if (!user) return;
		createBusy = true;
		createError = null;
		try {
			const r = await graphql<CreateSavedSearchMutation, CreateSavedSearchVariables>(
				CREATE_SAVED_SEARCH,
				{ input: { owner: user.id, ...v, visibility: 'SECRET' } }
			);
			await goto(r.createSavedSearch.url);
		} catch (err) {
			createError = errorMessage(err);
		} finally {
			createBusy = false;
		}
	}

	function formatDate(s: string): string {
		return new Date(s).toLocaleDateString(undefined, { year: 'numeric', month: 'short', day: 'numeric' });
	}
</script>

<svelte:head><title>Saved searches · Sourcegull</title></svelte:head>

<div class="page">
	<div class="page-header">
		<div>
			<h1>Saved searches</h1>
			<p class="page-description">
				Queries you run often, under a name, one click away. To be notified when new results appear, create a
				<a href="/watches">watch</a> from the query.
			</p>
		</div>
		{#if user && !creating}
			<div class="page-actions">
				<button type="button" class="btn btn-primary" onclick={() => (creating = true)}>
					<Icon name="plus" size={16} />New saved search
				</button>
			</div>
		{/if}
	</div>

	{#if creating}
		<section class="card new">
			<div class="card-header"><h2>New saved search</h2></div>
			<SavedSearchForm
				initial={{ description: '', query: prefill ?? 'patternType:keyword ', draft: false }}
				submitLabel="Create"
				error={createError}
				busy={createBusy}
				onsubmit={create}
				oncancel={() => (creating = false)}
			/>
			<p class="muted small hint">
				New saved searches are secret: only you (and site admins) can see them until you make them public.
			</p>
		</section>
	{/if}

	<div class="filter-bar">
		<label class="visually-hidden" for="ss-filter">Filter saved searches</label>
		<input id="ss-filter" class="input" type="search" placeholder="Filter by description or query…" bind:value={filter} {oninput} />
		{#if user}
			<div class="btn-group" role="group" aria-label="Owner">
				<button
					type="button"
					class="btn"
					aria-pressed={!mineOnly}
					onclick={() => {
						mineOnly = false;
						void load(null);
					}}>All</button
				>
				<button
					type="button"
					class="btn"
					aria-pressed={mineOnly}
					onclick={() => {
						mineOnly = true;
						void load(null);
					}}>Mine only</button
				>
			</div>
		{/if}
		<label class="visually-hidden" for="ss-order">Sort by</label>
		<select id="ss-order" class="input" bind:value={orderBy} onchange={() => load(null)}>
			<option value="SAVED_SEARCH_UPDATED_AT">Recently updated</option>
			<option value="SAVED_SEARCH_DESCRIPTION">Description</option>
		</select>
		<span class="muted small count">{totalCount} total</span>
	</div>

	{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}

	{#if items.length === 0 && !loading && !error && !filter.trim() && !mineOnly}
		<div class="empty-state">
			<span class="icon"><Icon name="bookmark" size={32} /></span>
			<h3>No saved searches yet</h3>
			<p>Use “Save search” on a results page, or create one here.</p>
			{#if user && !creating}
				<button type="button" class="btn btn-primary" onclick={() => (creating = true)}>
					<Icon name="plus" size={16} />New saved search
				</button>
			{/if}
		</div>
	{:else}
		<ul class="list list-group" aria-busy={loading}>
			{#each items as s (s.id)}
				<li>
					<div class="list-item-title">
						<a href={s.url}>{s.description}</a>
						{#if s.draft}<span class="badge badge-warning">draft</span>{/if}
						<span class="badge" class:badge-primary={s.visibility === 'PUBLIC'}>{s.visibility.toLowerCase()}</span>
						<span class="push"><a class="btn btn-sm" href={searchQueryURL(s.query)}>Run</a></span>
					</div>
					<code class="query mono">{s.query}</code>
					<p class="list-item-meta">
						{namespaceLabel(s.owner)} · updated {formatDate(s.updatedAt)}
					</p>
				</li>
			{:else}
				<li class="empty">
					{loading ? 'Loading…' : 'No saved searches match.'}
				</li>
			{/each}
		</ul>
	{/if}

	{#if hasMore}
		<div class="more">
			<button type="button" class="btn" disabled={loading} onclick={() => load(cursor)}>
				{loading ? 'Loading…' : 'Show more'}
			</button>
		</div>
	{/if}
</div>

<style>
	.new {
		margin-bottom: var(--space-6);
	}
	.hint {
		margin: var(--space-3) 0 0;
	}
	.count {
		margin-left: auto;
	}
	.query {
		font-size: var(--text-sm);
		color: var(--color-text-muted);
		overflow-wrap: anywhere;
	}
	.more {
		margin-top: var(--space-4);
		text-align: center;
	}
</style>
