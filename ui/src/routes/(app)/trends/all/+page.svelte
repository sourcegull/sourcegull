<script lang="ts">
	import { page } from '$app/state';
	import Icon from '#lib/components/Icon.svelte';
	import { onMount } from 'svelte';
	import { logEvent } from '#lib/events.ts';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import InsightCard from '#lib/insights/InsightCard.svelte';
	import InsightsHeader from '#lib/insights/InsightsHeader.svelte';
	import { INSIGHT_VIEW_IDS, type InsightViewIdsResult } from '#lib/insights/queries.ts';

	const PAGE_SIZE = 30;

	const dashboardId = $derived(page.url.searchParams.get('dashboardId'));

	let ids = $state.raw<string[]>([]);
	let totalCount = $state<number | null>(null);
	let cursor = $state<string | null>(null);
	let hasMore = $state(false);
	let loading = $state(true);
	let error = $state<string | null>(null);

	async function load(after: string | null): Promise<void> {
		loading = true;
		error = null;
		try {
			const r = await graphql<InsightViewIdsResult, { first: number; after: string | null }>(INSIGHT_VIEW_IDS, {
				first: PAGE_SIZE,
				after
			});
			const got = r.insightViews.nodes.filter((n): n is { id: string } => n !== null).map((n) => n.id);
			ids = after ? [...ids, ...got.filter((id) => !ids.includes(id))] : got;
			totalCount = r.insightViews.totalCount;
			cursor = r.insightViews.pageInfo.endCursor;
			hasMore = r.insightViews.pageInfo.hasNextPage;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loading = false;
		}
	}

	onMount(() => {
		logEvent('ViewInsights');
		void load(null);
	});

	function drop(id: string): void {
		ids = ids.filter((i) => i !== id);
		if (totalCount !== null) totalCount -= 1;
	}
</script>

<svelte:head><title>All trends · Trends · Sourcegull</title></svelte:head>

<div class="page insights-page">
	<InsightsHeader tab="all" {dashboardId} />

	{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}

	{#if ids.length > 0}
		<div class="grid" role="list" aria-label="Trends">
			{#each ids as id (id)}
				<div role="listitem" class="cell">
					<InsightCard {id} dashboard={null} virtualDashboard ondeleted={drop} onviewcreated={() => load(null)} />
				</div>
			{/each}
		</div>
		{#if hasMore}
			<div class="more">
				<button type="button" class="btn" disabled={loading} onclick={() => load(cursor)}>
					{loading ? 'Loading…' : 'Show more'}
				</button>
				{#if totalCount !== null}<span class="muted">{ids.length} of {totalCount}</span>{/if}
			</div>
		{/if}
	{:else if loading}
		<p class="loading" role="status"><span class="spinner"></span>Loading…</p>
	{:else if !error}
		<section class="empty-state">
			<span class="icon"><Icon name="insights" size={32} /></span>
			<h2>No trends yet</h2>
			<p>Trends you create, and those on dashboards you can see, appear here.</p>
			<a class="btn btn-primary" href="/trends/create"><Icon name="plus" size={16} />Create trend</a>
		</section>
	{/if}
</div>

<style>
	.insights-page {
		max-width: 90rem;
	}
	.grid {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(min(100%, 26rem), 1fr));
		gap: var(--space-4);
	}
	.cell {
		display: flex;
		min-width: 0;
	}
	.cell > :global(*) {
		flex: 1;
	}
	.more {
		display: flex;
		align-items: center;
		justify-content: center;
		gap: var(--space-3);
		margin-top: var(--space-4);
	}
</style>
