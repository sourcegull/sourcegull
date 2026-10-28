<script lang="ts">
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import type { PatternType } from './query.ts';
	import {
		SEARCH_AGGREGATE,
		type AggregationMode,
		type AggregationResult,
		type SearchAggregateResult,
		type SearchAggregateVariables
	} from './queries.ts';

	interface Props {
		/** The full query sent to the stream (context included). */
		query: string;
		patternType: PatternType;
		/** Runs a group's drill-down query. */
		onselect: (query: string) => void;
	}

	let { query, patternType, onselect }: Props = $props();

	const LIMIT = 10;
	const MODE_LABELS: Record<AggregationMode, string> = {
		REPO: 'Repository',
		PATH: 'File',
		AUTHOR: 'Author',
		CAPTURE_GROUP: 'Capture group'
	};

	let mode = $state<AggregationMode | null>(null);
	let extended = $state(false);
	let result = $state.raw<SearchAggregateResult['searchQueryAggregate'] | null>(null);
	let loading = $state(false);
	let error = $state<string | null>(null);
	let seq = 0;

	async function load(q: string, pt: PatternType, m: AggregationMode | null, ext: boolean): Promise<void> {
		const mine = ++seq;
		loading = true;
		error = null;
		try {
			const r = await graphql<SearchAggregateResult, SearchAggregateVariables>(SEARCH_AGGREGATE, {
				query: q,
				patternType: pt,
				mode: m,
				limit: LIMIT,
				extendedTimeout: ext
			});
			if (mine !== seq) return;
			result = r.searchQueryAggregate;
		} catch (err) {
			if (mine === seq) error = errorMessage(err);
		} finally {
			if (mine === seq) loading = false;
		}
	}

	// A new query starts from the server's default mode.
	let lastQuery = '';
	$effect(() => {
		const q = query;
		const pt = patternType;
		if (q !== lastQuery) {
			lastQuery = q;
			mode = null;
			extended = false;
		}
		void load(q, pt, mode, extended);
	});

	const agg: AggregationResult | null = $derived(result?.aggregations ?? null);
	const groups = $derived(agg && agg.__typename !== 'SearchAggregationNotAvailable' ? agg.groups : []);
	const max = $derived(Math.max(1, ...groups.map((g) => g.count)));
	const activeMode = $derived(mode ?? agg?.mode ?? null);
	const other = $derived.by(() => {
		if (!agg || agg.__typename === 'SearchAggregationNotAvailable' || !agg.otherResultCount) return '';
		const groups =
			agg.__typename === 'ExhaustiveSearchAggregationResult'
				? String(agg.otherGroupCount ?? 0)
				: `about ${agg.approximateOtherGroupCount ?? 0}`;
		return `${agg.otherResultCount} more results in ${groups} other groups`;
	});
</script>

<section class="aggregations card" aria-busy={loading}>
	<div class="modes" role="tablist" aria-label="Group results by">
		<span class="muted">Group by</span>
		{#each result?.modeAvailability ?? [] as m (m.mode)}
			<button
				type="button"
				role="tab"
				class="btn btn-sm"
				aria-selected={activeMode === m.mode}
				disabled={!m.available}
				title={m.reasonUnavailable ?? undefined}
				onclick={() => (mode = m.mode)}
			>
				{MODE_LABELS[m.mode]}
			</button>
		{/each}
	</div>

	{#if error}
		<div class="alert alert-danger" role="alert">{error}</div>
	{:else if agg?.__typename === 'SearchAggregationNotAvailable'}
		<p class="muted">{agg.reason}</p>
		{#if agg.reasonType === 'TIMEOUT_EXTENSION_AVAILABLE' && !extended}
			<button type="button" class="btn btn-sm" onclick={() => (extended = true)}>Run with a longer timeout</button>
		{/if}
	{:else if groups.length === 0}
		<p class="muted">{loading ? 'Grouping results…' : 'No results to group.'}</p>
	{:else}
		<ol class="bars">
			{#each groups as g (g.label)}
				<li>
					<button
						type="button"
						class="bar-row"
						disabled={!g.query}
						title={g.query ?? g.label}
						onclick={() => g.query && onselect(g.query)}
					>
						<span class="bar-label mono">{g.label}</span>
						<span class="bar-track"><span class="bar" style:width="{(100 * g.count) / max}%"></span></span>
						<span class="bar-count">{g.count}</span>
					</button>
				</li>
			{/each}
		</ol>
		{#if other}
			<p class="muted other">{other}</p>
		{/if}
	{/if}
</section>

<style>
	.aggregations {
		padding: var(--space-3);
		display: grid;
		gap: var(--space-2);
	}
	.modes {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: var(--space-2);
		font-size: 0.85rem;
	}
	.modes [aria-selected='true'] {
		background: var(--color-info-bg);
		border-color: var(--color-primary);
		color: var(--color-primary);
	}
	.bars {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 2px;
	}
	.bar-row {
		display: grid;
		grid-template-columns: minmax(8rem, 2fr) 3fr 4rem;
		align-items: center;
		gap: var(--space-2);
		width: 100%;
		padding: 2px var(--space-1);
		border: 0;
		border-radius: var(--radius);
		background: none;
		color: var(--color-text);
		font: inherit;
		font-size: 0.85rem;
		text-align: left;
		cursor: pointer;
	}
	.bar-row:hover:not(:disabled) {
		background: var(--color-bg-muted);
	}
	.bar-row:disabled {
		cursor: default;
	}
	.bar-label {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.bar-track {
		height: 0.7rem;
		background: var(--color-bg-muted);
		border-radius: 3px;
		overflow: hidden;
	}
	.bar {
		display: block;
		height: 100%;
		background: var(--color-primary);
		border-radius: 3px;
	}
	.bar-count {
		text-align: right;
		font-variant-numeric: tabular-nums;
		color: var(--color-text-muted);
	}
	.other {
		margin: 0;
		font-size: 0.85rem;
	}
</style>
