<!--
	Bulk changes, newest first, with a state filter: every namespace the viewer can
	see, or one user's or organization's. Each entry shows the namespace and name, state,
	description and changeset counts.
-->
<script lang="ts">
	import Icon from '#lib/components/Icon.svelte';
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		BATCH_CHANGES,
		ORG_BATCH_CHANGES,
		STATE_FILTERS,
		USER_BATCH_CHANGES,
		namespaceName,
		relativeTime,
		stateFilterStates,
		type BatchChangeConnection,
		type BatchChangeListNode,
		type BatchChangesResult,
		type OrgBatchChangesResult,
		type UserBatchChangesResult,
		type NamespaceKind
	} from './batch-change.ts';
	import Markdown from './Markdown.svelte';
	import StateBadge from './StateBadge.svelte';

	interface Props {
		/** One namespace's bulk changes; all the viewer can see without. */
		namespace?: { kind: NamespaceKind; name: string } | null;
	}

	let { namespace = null }: Props = $props();

	const PAGE = 20;

	const filter = $derived(STATE_FILTERS.find((f) => f.id === page.url.searchParams.get('state'))?.id ?? 'all');

	let nodes = $state.raw<BatchChangeListNode[]>([]);
	let totalCount = $state(0);
	let cursor = $state<string | null>(null);
	let hasNext = $state(false);
	let loading = $state(true);
	let error = $state<string | null>(null);

	async function fetchPage(after: string | null): Promise<BatchChangeConnection | null> {
		const vars = { first: PAGE, after, states: stateFilterStates(filter) };
		if (!namespace) {
			return (await graphql<BatchChangesResult, typeof vars>(BATCH_CHANGES, vars)).batchChanges;
		}
		if (namespace.kind === 'users') {
			const r = await graphql<UserBatchChangesResult, typeof vars & { user: string }>(USER_BATCH_CHANGES, {
				...vars,
				user: namespace.name
			});
			return r.user?.batchChanges ?? null;
		}
		const r = await graphql<OrgBatchChangesResult, typeof vars & { org: string }>(ORG_BATCH_CHANGES, {
			...vars,
			org: namespace.name
		});
		return r.organization?.batchChanges ?? null;
	}

	async function load(more = false): Promise<void> {
		loading = true;
		error = null;
		try {
			const conn = await fetchPage(more ? cursor : null);
			if (!conn) {
				error = 'Namespace not found.';
				nodes = [];
				return;
			}
			nodes = more ? [...nodes, ...conn.nodes] : conn.nodes;
			totalCount = conn.totalCount;
			cursor = conn.pageInfo.endCursor;
			hasNext = conn.pageInfo.hasNextPage;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loading = false;
		}
	}

	$effect(() => {
		void filter;
		void namespace;
		void load();
	});

	function selectFilter(id: string): void {
		const url = new URL(page.url.href);
		if (id === 'all') url.searchParams.delete('state');
		else url.searchParams.set('state', id);
		void goto(url, { replaceState: true });
	}

	const stateTone = (s: string) => (s === 'OPEN' ? 'success' : s === 'CLOSED' ? 'danger' : 'muted');
</script>

<div class="filters btn-group" role="tablist" aria-label="State">
	{#each STATE_FILTERS as f (f.id)}
		<button
			class="btn btn-sm"
			type="button"
			role="tab"
			aria-selected={filter === f.id}
			onclick={() => selectFilter(f.id)}>{f.title}</button
		>
	{/each}
</div>

{#if error}
	<div class="alert alert-danger" role="alert">{error}</div>
{/if}

<ul class="batch-changes list-group" aria-busy={loading}>
	{#each nodes as bc (bc.id)}
		{@const s = bc.changesetsStats}
		<li class="batch-change" data-batch-change={bc.name}>
			<div class="main">
				<div class="title">
					<StateBadge state={bc.state} tone={stateTone(bc.state)} />
					<span class="ns">{namespaceName(bc.namespace)} /</span>
					<a class="name" href={bc.url}>{bc.name}</a>
				</div>
				{#if bc.description}
					<div class="description"><Markdown source={bc.description.split('\n\n')[0] ?? ''} /></div>
				{/if}
				<div class="muted small">
					{#if bc.state === 'CLOSED' && bc.closedAt}
						Closed {relativeTime(bc.closedAt)}
					{:else}
						Created {relativeTime(bc.createdAt)}{bc.creator ? ` by ${bc.creator.username}` : ''}
					{/if}
				</div>
			</div>
			<dl class="counts">
				<div><dt>Open</dt><dd>{s.open}</dd></div>
				<div><dt>Merged</dt><dd>{s.merged}</dd></div>
				<div><dt>Closed</dt><dd>{s.closed}</dd></div>
				<div><dt>Total</dt><dd>{s.total}</dd></div>
			</dl>
		</li>
	{:else}
		{#if !loading && !error}
			<li class="empty">
				{#if filter === 'all'}
					<span class="icon"><Icon name="batch" size={32} /></span>
					<strong>No bulk changes yet.</strong>
					<span>Describe a change once and Sourcegull opens a merge request in every matching repository.</span>
					<a class="btn btn-primary" href="/bulk-changes/create"><Icon name="plus" size={16} />Create bulk change</a>
				{:else}
					No {filter} bulk changes.
				{/if}
			</li>
		{/if}
	{/each}
</ul>

{#if nodes.length > 0 || hasNext}
	<div class="footer">
		<span class="muted small">{totalCount} {totalCount === 1 ? 'bulk change' : 'bulk changes'}</span>
		{#if hasNext}
			<button class="btn btn-sm" type="button" disabled={loading} onclick={() => load(true)}>Show more</button>
		{/if}
	</div>
{/if}

<style>
	.filters {
		flex-wrap: wrap;
		margin-bottom: var(--space-4);
	}
	.batch-change {
		display: flex;
		gap: var(--space-4);
		align-items: flex-start;
	}
	.batch-change:hover {
		background: var(--color-bg-hover);
	}
	.main {
		flex: 1;
		min-width: 0;
	}
	.title {
		display: flex;
		align-items: baseline;
		gap: var(--space-2);
		flex-wrap: wrap;
	}
	.ns {
		color: var(--color-text-muted);
	}
	.name {
		font-weight: var(--weight-semibold);
		color: var(--color-text);
		overflow-wrap: anywhere;
	}
	.description {
		margin: var(--space-1) 0;
		max-height: 4.5em;
		overflow: hidden;
	}
	.counts {
		display: flex;
		gap: var(--space-4);
		margin: 0;
	}
	.counts div {
		text-align: center;
	}
	.counts dt {
		font-size: var(--text-xs);
		color: var(--color-text-muted);
	}
	.counts dd {
		margin: 0;
		font-weight: 600;
		font-variant-numeric: tabular-nums;
	}
	.empty {
		justify-items: center;
		gap: var(--space-2);
	}
	.empty .icon {
		color: var(--color-text-subtle);
	}
	.empty strong {
		color: var(--color-text);
	}
	.footer {
		display: flex;
		justify-content: space-between;
		align-items: center;
		margin-top: var(--space-3);
	}
	@media (max-width: 40rem) {
		.batch-change {
			flex-direction: column;
			gap: var(--space-2);
		}
	}
</style>
