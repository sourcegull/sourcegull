<script lang="ts">
	import { onMount } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		BUMP_DERIVATIVE_GRAPH_KEY,
		RANKING_SUMMARY,
		type RankingSummaryResult
	} from '#lib/queries.ts';
	import { rankingStatus } from './ranking.ts';

	let summary = $state<RankingSummaryResult['rankingSummary'] | null>(null);
	let error = $state<string | null>(null);
	let requested = $state(false);

	async function load(): Promise<void> {
		try {
			summary = (await graphql<RankingSummaryResult>(RANKING_SUMMARY)).rankingSummary;
			error = null;
		} catch (err) {
			error = errorMessage(err);
		}
	}

	async function recompute(): Promise<void> {
		try {
			await graphql(BUMP_DERIVATIVE_GRAPH_KEY);
			requested = true;
		} catch (err) {
			error = errorMessage(err);
		}
	}

	onMount(() => void load());
</script>

<p class="ranking" data-testid="ranking-status">
	<strong>Ranking:</strong>
	{#if error}
		<span class="text-danger">{error}</span>
	{:else if summary}
		{@const status = rankingStatus(summary, new Date())}
		<span class:text-danger={status.failed}>{status.text}</span>
		{#if summary.enabled}
			{#if requested}
				<span class="muted">Recomputing at the next pass.</span>
			{:else}
				<button class="btn btn-sm" onclick={() => void recompute()}>Recompute</button>
			{/if}
		{/if}
	{:else}
		<span class="muted">Loading…</span>
	{/if}
</p>

<style>
	.ranking {
		display: flex;
		gap: 0.5rem;
		align-items: baseline;
		flex-wrap: wrap;
	}
</style>
