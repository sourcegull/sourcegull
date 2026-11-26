<script lang="ts">
	import { diffLines, splitDiffLines, type DiffLine } from './diff.ts';
	import type { FileDiffNode } from './queries.ts';
	let { hunks }: { hunks: FileDiffNode['hunks'] } = $props();
</script>

{#snippet cells(line: DiffLine | null, side: 'old' | 'new')}
	<td class="num {line?.kind ?? 'empty'}">{(side === 'old' ? line?.oldLine : line?.newLine) ?? ''}</td>
	<td class="code {line?.kind ?? 'empty'}"
		><span class="prefix">{line?.kind === 'deleted' ? '−' : line?.kind === 'added' ? '+' : ''}</span>{line?.text ??
			''}</td
	>
{/snippet}

<table class="mono" aria-label="Side-by-side diff">
	<colgroup><col class="number" /><col /><col class="number" /><col /></colgroup>
	<thead><tr><th colspan="2" scope="colgroup">Before</th><th colspan="2" scope="colgroup">After</th></tr></thead>
	<tbody>
		{#each hunks as h, i (i)}
			<tr class="range"
				><td colspan="2">@@ −{h.oldRange.startLine},{h.oldRange.lines}</td><td colspan="2"
					>+{h.newRange.startLine},{h.newRange.lines} @@</td
				></tr
			>
			{#each splitDiffLines(diffLines(h.body, h.oldRange.startLine, h.newRange.startLine)) as row, j (j)}
				<tr>{@render cells(row.old, 'old')}{@render cells(row.new, 'new')}</tr>
			{/each}
		{/each}
	</tbody>
</table>

<style>
	table {
		border-collapse: collapse;
		table-layout: fixed;
		width: 100%;
		font-size: var(--text-xs);
		line-height: 1.6;
	}
	.number {
		width: 3.25rem;
	}
	th,
	td {
		text-align: left;
		vertical-align: top;
		padding: 0 var(--space-2);
	}
	th {
		background: var(--color-bg-subtle);
		color: var(--color-text-muted);
		padding-block: var(--space-1);
	}
	td:nth-child(3),
	th + th,
	.range td + td {
		border-left: 1px solid var(--color-border);
	}
	.range {
		background: var(--color-info-bg);
		color: var(--color-text-muted);
	}
	.num {
		text-align: right;
		user-select: none;
		color: var(--color-text-subtle);
		background: var(--editor-gutter);
	}
	.code {
		white-space: pre-wrap;
		overflow-wrap: anywhere;
	}
	.prefix {
		user-select: none;
		margin-right: 0.4em;
		color: var(--color-text-muted);
	}
	.added {
		background: var(--diff-added-bg);
	}
	.deleted {
		background: var(--diff-deleted-bg);
	}
	.note {
		font-style: italic;
		color: var(--color-text-muted);
	}
	.empty {
		background: var(--color-bg-subtle);
	}
</style>
