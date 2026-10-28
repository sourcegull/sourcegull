<script lang="ts">
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import InsightCard from '#lib/insights/InsightCard.svelte';
	import { insightURL } from '#lib/insights/insights.ts';

	const id = $derived(page.params.id ?? '');
</script>

<svelte:head><title>Trend · Sourcegull</title></svelte:head>

<div class="page insight-page">
	<nav class="crumbs" aria-label="Breadcrumb"><a href="/trends">Trends</a> / <span>Trend</span></nav>
	{#key id}
		<InsightCard
			{id}
			dashboard={null}
			ondeleted={() => goto('/trends', { replaceState: true })}
			onviewcreated={(newId) => goto(insightURL(newId))}
		/>
	{/key}
</div>

<style>
	.insight-page {
		max-width: 60rem;
		display: flex;
		flex-direction: column;
		gap: var(--space-3);
	}
	.insight-page > :global(.insight-card) {
		min-height: 26rem;
	}
</style>
