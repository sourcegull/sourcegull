<script lang="ts">
	import { formatNumber } from '#lib/format.svelte.ts';
	import {
		hasNextPage,
		hasPreviousPage,
		nextPage,
		pageIndex,
		previousPage,
		type CursorPages
	} from '#lib/cursor-pages.ts';

	/**
	 * "1–50 of 117" with Previous and Next, for a list paged by `pages`. Put it above the list:
	 * there it stays in place whatever the page length, so repeated clicks never land on a row.
	 */
	let {
		pages,
		shown,
		total,
		loading = false,
		onchange
	}: {
		pages: CursorPages;
		/** Rows on the current page. */
		shown: number;
		/** Rows on all pages. */
		total: number;
		loading?: boolean;
		/** Called after the page changed; load the new page here. */
		onchange: () => void;
	} = $props();

	const fmt = (n: number) => formatNumber(n);
	const start = $derived(pageIndex(pages) * pages.size);
</script>

<span class="pager">
	<span class="muted">
		{#if total > 0}{fmt(start + 1)}–{fmt(start + shown)} of {fmt(total)}{/if}
	</span>
	<button
		class="btn btn-sm"
		type="button"
		disabled={!hasPreviousPage(pages) || loading}
		onclick={() => {
			previousPage(pages);
			onchange();
		}}>Previous</button
	>
	<button
		class="btn btn-sm"
		type="button"
		disabled={!hasNextPage(pages) || loading}
		onclick={() => {
			nextPage(pages);
			onchange();
		}}>Next</button
	>
</span>

<style>
	.pager {
		display: inline-flex;
		align-items: center;
		gap: var(--space-2);
		margin-left: auto;
	}
</style>
