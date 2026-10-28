<!--
	A query block's results: the search runs through the streaming API as soon as the block is
	shown (and again on "Run"), and matches render with the search page's result component.
-->
<script lang="ts">
	import SearchResult from '#lib/search/SearchResult.svelte';
	import { parsePatternType, searchQueryURL, searchURL, type PatternType } from '#lib/search/query.ts';
	import { matchCount, matchKey, streamSearch, type Alert, type Progress, type SearchMatch } from '#lib/search/stream.ts';

	interface Props {
		query: string;
		/** The notebook's default pattern type, for queries without `patternType:`. */
		patternType: string;
		/** Bumped to run the query again. */
		runKey?: number;
	}

	let { query, patternType, runKey = 0 }: Props = $props();

	/** Matches kept (and sent by the server) per block. */
	const DISPLAY = 50;
	/** Matches shown before "Show all". */
	const COLLAPSED = 5;

	let matches = $state.raw<SearchMatch[]>([]);
	let progress = $state.raw<Progress | null>(null);
	let alert = $state.raw<Alert | null>(null);
	let error = $state<string | null>(null);
	let running = $state(false);
	let showAll = $state(false);

	const pt = $derived<PatternType>(parsePatternType(patternType));

	$effect(() => {
		const q = query.trim();
		void runKey;
		const t = pt;
		matches = [];
		progress = null;
		alert = null;
		error = null;
		showAll = false;
		if (!q) {
			running = false;
			return;
		}
		const controller = new AbortController();
		running = true;
		void streamSearch(
			{ query: q, patternType: t, display: DISPLAY },
			{
				onMatches: (m) => {
					matches = [...matches, ...m].slice(0, DISPLAY);
				},
				onProgress: (p) => (progress = p),
				onAlert: (a) => (alert = a),
				onError: (m) => (error = m),
				onDone: () => (running = false)
			},
			controller.signal
		).finally(() => {
			if (!controller.signal.aborted) running = false;
		});
		return () => controller.abort();
	});

	const total = $derived(matches.reduce((n, m) => n + matchCount(m), 0));
	const openURL = $derived(/(^|\s)patterntype:/i.test(query) ? searchQueryURL(query) : searchURL(query, pt));
</script>

<div class="results" aria-busy={running}>
	<div class="summary muted">
		{#if running}
			Searching… {total} results so far
		{:else if progress}
			{progress.matchCount} results in {(progress.durationMs / 1000).toFixed(2)}s
			{#if progress.skipped.length > 0}· {progress.skipped.map((s) => s.title).join(', ')}{/if}
		{/if}
		<span class="spacer"></span>
		{#if query.trim()}<a href={openURL}>Open in search</a>{/if}
	</div>
	{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}
	{#if alert}
		<div class="alert alert-info" role="status"><strong>{alert.title}</strong> {alert.description}</div>
	{/if}
	{#each showAll ? matches : matches.slice(0, COLLAPSED) as m, i (matchKey(m, i))}
		<SearchResult match={m} />
	{/each}
	{#if !running && !error && matches.length === 0 && query.trim()}
		<p class="muted">No results.</p>
	{/if}
	{#if !showAll && matches.length > COLLAPSED}
		<button type="button" class="btn btn-sm" onclick={() => (showAll = true)}>
			Show {matches.length - COLLAPSED} more
		</button>
	{/if}
</div>

<style>
	.results {
		display: grid;
		gap: var(--space-2);
	}
	.summary {
		display: flex;
		gap: var(--space-2);
		font-size: 0.85rem;
	}
	.spacer {
		flex: 1;
	}
	.alert {
		margin: 0;
	}
</style>
