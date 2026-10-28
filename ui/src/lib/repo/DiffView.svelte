<script lang="ts">
	import { SvelteSet } from 'svelte/reactivity';
	import Icon from '#lib/components/Icon.svelte';
	import { diffLines } from './diff.ts';
	import type { FileDiffConnection, FileDiffNode } from './queries.ts';
	import { repoPageURL } from './url.ts';

	interface Props {
		repo: string;
		diffs: FileDiffConnection;
		/** Revision the new side is browsable at; without one, paths are not links. */
		headRev?: string | null;
	}

	let { repo, diffs, headRev }: Props = $props();

	/** Files with more changed lines start collapsed. */
	const COLLAPSE_LINES = 400;

	const collapsed = new SvelteSet<string>();
	$effect(() => {
		collapsed.clear();
		for (const f of diffs.nodes) {
			if (f.stat.added + f.stat.deleted > COLLAPSE_LINES) collapsed.add(key(f));
		}
	});

	function key(f: FileDiffNode): string {
		return `${f.oldPath ?? ''}\u0000${f.newPath ?? ''}`;
	}

	function title(f: FileDiffNode): string {
		if (f.oldPath && f.newPath && f.oldPath !== f.newPath) return `${f.oldPath} → ${f.newPath}`;
		return f.newPath ?? f.oldPath ?? '';
	}

	function toggle(f: FileDiffNode): void {
		const k = key(f);
		if (collapsed.has(k)) collapsed.delete(k);
		else collapsed.add(k);
	}
</script>

<div class="diff-summary">
	<strong>{diffs.totalCount} {diffs.totalCount === 1 ? 'file' : 'files'} changed</strong>
	<span class="added">+{diffs.diffStat.added}</span>
	<span class="deleted">−{diffs.diffStat.deleted}</span>
	{#if diffs.nodes.length < diffs.totalCount}
		<span class="muted">(showing {diffs.nodes.length})</span>
	{/if}
</div>

{#each diffs.nodes as f (key(f))}
	<section class="file-diff card">
		<header>
			<button
				type="button"
				class="btn btn-ghost btn-icon btn-sm toggle"
				class:open={!collapsed.has(key(f))}
				aria-expanded={!collapsed.has(key(f))}
				aria-label={collapsed.has(key(f)) ? 'Expand file' : 'Collapse file'}
				onclick={() => toggle(f)}><Icon name="chevron-right" size={14} /></button
			>
			<span class="badge status-{f.status.toLowerCase()}">{f.status.toLowerCase().replace('_', ' ')}</span>
			{#if f.url}
				<a class="mono path" href={f.url}>{title(f)}</a>
			{:else if f.newPath && headRev}
				<a class="mono path" href={repoPageURL(repo, headRev, 'blob', f.newPath)}>{title(f)}</a>
			{:else}
				<span class="mono path">{title(f)}</span>
			{/if}
			<span class="spacer"></span>
			<span class="stat">
				<span class="added">+{f.stat.added}</span>
				<span class="deleted">−{f.stat.deleted}</span>
			</span>
		</header>
		{#if !collapsed.has(key(f))}
			{#if f.binary}
				<p class="muted note">Binary file changed.</p>
			{:else if f.tooLarge}
				<p class="muted note">The diff is too large to show.</p>
			{:else if f.hunks.length === 0}
				<p class="muted note">No content changes.</p>
			{:else}
				<table class="hunks mono">
					<tbody>
						{#each f.hunks as h, hi (hi)}
							<tr class="hunk-header">
								<td colspan="3">@@ -{h.oldRange.startLine},{h.oldRange.lines} +{h.newRange.startLine},{h.newRange.lines} @@</td>
							</tr>
							{#each diffLines(h.body, h.oldRange.startLine, h.newRange.startLine) as l, li (li)}
								<tr class="line {l.kind}">
									<td class="num">{l.oldLine ?? ''}</td>
									<td class="num">{l.newLine ?? ''}</td>
									<td class="text"
										><span class="prefix" aria-hidden="true"
											>{l.kind === 'added' ? '+' : l.kind === 'deleted' ? '-' : ' '}</span
										>{l.text}</td
									>
								</tr>
							{/each}
						{/each}
					</tbody>
				</table>
			{/if}
		{/if}
	</section>
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
	.file-diff {
		padding: 0;
		margin-bottom: var(--space-3);
		overflow: hidden;
	}
	.file-diff header {
		position: sticky;
		top: 0;
		z-index: 1;
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: var(--space-2);
		padding: var(--space-1) var(--space-3) var(--space-1) var(--space-1);
		background: var(--color-bg-subtle);
		border-bottom: 1px solid var(--color-border);
		font-size: var(--text-sm);
	}
	.toggle {
		color: var(--color-text-muted);
	}
	.toggle :global(.icon) {
		transition: transform var(--duration) var(--ease);
	}
	.toggle.open :global(.icon) {
		transform: rotate(90deg);
	}
	.path {
		overflow-wrap: anywhere;
		min-width: 0;
		color: var(--color-text);
		font-weight: var(--weight-medium);
	}
	a.path:hover {
		color: var(--color-link);
	}
	.spacer {
		flex: 1;
	}
	.stat {
		display: inline-flex;
		gap: var(--space-2);
		font-size: var(--text-xs);
	}
	.status-added {
		background: var(--color-success-bg);
		color: var(--color-success);
	}
	.status-deleted {
		background: var(--color-danger-bg);
		color: var(--color-danger);
	}
	.note {
		padding: var(--space-3);
		margin: 0;
	}
	.hunks {
		width: 100%;
		border-collapse: collapse;
		font-size: var(--text-xs);
		line-height: 1.6;
	}
	.hunks td {
		padding: 0 var(--space-2);
		white-space: pre;
		vertical-align: top;
	}
	.hunk-header td {
		background: var(--color-info-bg);
		color: var(--color-text-muted);
		padding: 0.2rem var(--space-3);
	}
	.num {
		width: 1%;
		min-width: 2.75rem;
		text-align: right;
		color: var(--color-text-subtle);
		background: var(--editor-gutter);
		user-select: none;
		border-right: 1px solid var(--color-border);
		font-variant-numeric: tabular-nums;
	}
	.text {
		width: 98%;
		white-space: pre-wrap !important;
		word-break: break-all;
	}
	.prefix {
		user-select: none;
		color: var(--color-text-subtle);
		margin-right: 0.4rem;
	}
	.line.added td {
		background: var(--diff-added-bg);
	}
	.line.deleted td {
		background: var(--diff-deleted-bg);
	}
	.line.added .num {
		background: color-mix(in srgb, var(--diff-added-text) 12%, var(--diff-added-bg));
	}
	.line.deleted .num {
		background: color-mix(in srgb, var(--diff-deleted-text) 12%, var(--diff-deleted-bg));
	}
	.line.added .prefix {
		color: var(--diff-added-text);
	}
	.line.deleted .prefix {
		color: var(--diff-deleted-text);
	}
	.line.note td {
		color: var(--color-text-muted);
		font-style: italic;
	}
</style>
