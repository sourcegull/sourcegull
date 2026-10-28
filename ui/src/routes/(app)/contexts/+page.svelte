<script lang="ts">
	import Icon from '#lib/components/Icon.svelte';
	import ManagedBadge from '#lib/components/ManagedBadge.svelte';
	import { page } from '$app/state';
	import { onMount } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		DELETE_SEARCH_CONTEXT,
		SEARCH_CONTEXTS,
		SET_DEFAULT_SEARCH_CONTEXT,
		STAR_SEARCH_CONTEXT,
		UNSTAR_SEARCH_CONTEXT,
		namespaceLabel,
		type SearchContextNode,
		type SearchContextsResult,
		type SearchContextsVariables,
		type StarVariables
	} from '#lib/search/queries.ts';
	import { quoteValue, searchURL } from '#lib/search/query.ts';

	const PAGE_SIZE = 50;

	const user = $derived(page.data.auth?.user ?? null);

	let items = $state.raw<SearchContextNode[]>([]);
	let totalCount = $state(0);
	let cursor = $state<string | null>(null);
	let hasMore = $state(false);
	let filter = $state('');
	let loading = $state(true);
	let error = $state<string | null>(null);
	let actionError = $state<string | null>(null);
	let seq = 0;

	async function load(after: string | null): Promise<void> {
		const mine = ++seq;
		loading = true;
		error = null;
		try {
			const r = await graphql<SearchContextsResult, SearchContextsVariables>(SEARCH_CONTEXTS, {
				first: PAGE_SIZE,
				after,
				query: filter.trim() || null
			});
			if (mine !== seq) return;
			items = after ? [...items, ...r.searchContexts.nodes] : r.searchContexts.nodes;
			totalCount = r.searchContexts.totalCount;
			cursor = r.searchContexts.pageInfo.endCursor;
			hasMore = r.searchContexts.pageInfo.hasNextPage;
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

	async function act(f: () => Promise<unknown>): Promise<void> {
		actionError = null;
		try {
			await f();
			await load(null);
		} catch (err) {
			actionError = errorMessage(err);
		}
	}

	function vars(c: SearchContextNode): StarVariables {
		return { searchContextID: c.id, userID: user?.id ?? '' };
	}

	const star = (c: SearchContextNode) =>
		act(() =>
			graphql<unknown, StarVariables>(c.viewerHasStarred ? UNSTAR_SEARCH_CONTEXT : STAR_SEARCH_CONTEXT, vars(c))
		);
	const makeDefault = (c: SearchContextNode) =>
		act(() => graphql<unknown, StarVariables>(SET_DEFAULT_SEARCH_CONTEXT, vars(c)));
	const remove = (c: SearchContextNode) => {
		if (!confirm(`Delete the search context ${c.spec}?`)) return;
		void act(() => graphql<unknown, { id: string }>(DELETE_SEARCH_CONTEXT, { id: c.id }));
	};

	function editURL(c: SearchContextNode): string {
		return `/contexts/edit?${new URLSearchParams({ spec: c.spec }).toString()}`;
	}
</script>

<svelte:head><title>Search contexts · Sourcegull</title></svelte:head>

<div class="page">
	<div class="page-header">
		<div>
			<h1>Search contexts</h1>
			<p class="page-description">
				Limit a search to a set of repositories (with revisions) or to what a query selects. Pick one in the search box or
				write <code class="code-inline">context:@user/name</code>.
			</p>
		</div>
		{#if user}
			<div class="page-actions">
				<a class="btn btn-primary" href="/contexts/new"><Icon name="plus" size={16} />New context</a>
			</div>
		{/if}
	</div>

	<div class="filter-bar">
		<label class="visually-hidden" for="ctx-filter">Filter contexts</label>
		<input id="ctx-filter" class="input" type="search" placeholder="Filter contexts…" bind:value={filter} {oninput} />
		<span class="muted small count">{totalCount} total</span>
	</div>

	{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}
	{#if actionError}<div class="alert alert-danger" role="alert">{actionError}</div>{/if}

	<div class="table-scroll">
		<table class="table table-bordered" aria-busy={loading}>
			<thead>
				<tr>
					<th scope="col">Context</th>
					<th scope="col">Owner</th>
					<th scope="col">Updated</th>
					<th scope="col"><span class="visually-hidden">Actions</span></th>
				</tr>
			</thead>
			<tbody>
				{#each items as c (c.id)}
					<tr>
						<td>
							<div class="spec">
								{#if user && !c.autoDefined}
									<button
										type="button"
										class="star"
										class:on={c.viewerHasStarred}
										aria-pressed={c.viewerHasStarred}
										title={c.viewerHasStarred ? 'Unstar' : 'Star'}
										onclick={() => star(c)}>{c.viewerHasStarred ? '★' : '☆'}</button
									>
								{/if}
								<a class="mono" href={searchURL(`context:${quoteValue(c.spec)} `)}>{c.spec}</a>
								{#if c.viewerHasAsDefault}<span class="badge badge-primary">default</span>{/if}
								{#if !c.public}<span class="badge">private</span>{/if}
								<ManagedBadge managedBy={c.managedBy} />
							</div>
							{#if c.description}<div class="muted desc">{c.description}</div>{/if}
							{#if c.query}<code class="query">{c.query}</code>{/if}
						</td>
						<td>{c.autoDefined ? 'Built in' : namespaceLabel(c.namespace)}</td>
						<td class="muted">{c.autoDefined ? '' : new Date(c.updatedAt).toLocaleDateString()}</td>
						<td class="actions">
							{#if user && !c.viewerHasAsDefault}
								<button type="button" class="btn btn-sm" onclick={() => makeDefault(c)}>Make default</button>
							{/if}
							{#if c.viewerCanManage && !c.autoDefined}
								<a class="btn btn-sm" href={editURL(c)}>Edit</a>
								<button type="button" class="btn btn-sm btn-danger" onclick={() => remove(c)}>Delete</button>
							{/if}
						</td>
					</tr>
				{:else}
					<tr><td colspan="4" class="muted">{loading ? 'Loading…' : 'No contexts match.'}</td></tr>
				{/each}
			</tbody>
		</table>
	</div>

	{#if hasMore}
		<div class="more">
			<button type="button" class="btn" disabled={loading} onclick={() => load(cursor)}>
				{loading ? 'Loading…' : 'Show more'}
			</button>
		</div>
	{/if}
</div>

<style>
	.count {
		margin-left: auto;
	}
	.more {
		margin-top: var(--space-4);
		text-align: center;
	}
	.spec {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		font-weight: 600;
	}
	.desc {
		font-size: var(--text-sm);
	}
	.query {
		font-size: var(--text-xs);
		color: var(--color-text-muted);
		overflow-wrap: anywhere;
	}
	.star {
		border: 0;
		background: none;
		cursor: pointer;
		font-size: var(--text-lg);
		line-height: 1;
		color: var(--color-text-subtle);
		padding: 0;
	}
	.star.on {
		color: var(--color-warning);
	}
	.actions {
		text-align: right;
		white-space: nowrap;
	}
	.actions > * + * {
		margin-left: var(--space-1);
	}
</style>
