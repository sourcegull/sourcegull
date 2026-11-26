<script lang="ts">
	import FilePatch from './FilePatch.svelte';
	import type { FileDiffConnection } from './queries.ts';

	let {
		repo,
		diffs,
		headRev
	}: {
		repo: string;
		diffs: FileDiffConnection;
		headRev?: string | null;
	} = $props();
</script>

<div class="diff-summary">
	<strong>{diffs.totalCount} {diffs.totalCount === 1 ? 'file' : 'files'} changed</strong>
	<span class="added">+{diffs.diffStat.added}</span>
	<span class="deleted">−{diffs.diffStat.deleted}</span>
	{#if diffs.nodes.length < diffs.totalCount}
		<span class="muted">(showing {diffs.nodes.length})</span>
	{/if}
</div>

{#each diffs.nodes as f ([f.oldPath, f.newPath].join('\u0000'))}
	<FilePatch {repo} {f} {headRev} initiallyCollapsed={f.stat.added + f.stat.deleted > 400} />
{/each}

<style>
	.diff-summary {
		display: flex;
		flex-wrap: wrap;
		gap: var(--space-3);
		align-items: baseline;
		margin: var(--space-5) 0 var(--space-3);
		font-size: var(--text-sm);
	}
	.added {
		color: var(--diff-added-text);
		font-weight: var(--weight-semibold);
		font-variant-numeric: tabular-nums;
	}
	.deleted {
		color: var(--diff-deleted-text);
		font-weight: var(--weight-semibold);
		font-variant-numeric: tabular-nums;
	}
</style>
