<script lang="ts">
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { graphql, errorMessage } from '#lib/graphql.ts';
	import type { CompareFileQuery, CompareFileQueryVariables } from '#lib/graphql-types.ts';
	import ChangedFileTree from './ChangedFileTree.svelte';
	import FilePatch from './FilePatch.svelte';
	import { COMPARE_FILE, type ChangedFile, type FileDiffNode } from './queries.ts';

	let {
		repo,
		files,
		truncated,
		base,
		head
	}: {
		repo: string;
		files: ChangedFile[];
		truncated: boolean;
		base: string;
		head: string;
	} = $props();
	const options = $derived(new URLSearchParams(page.url.hash.slice(1)));
	let path = $derived(options.get('file'));
	const selected = $derived(files.find((f) => (f.newPath ?? f.oldPath) === path) ?? files[0] ?? null);
	let split = $derived(options.get('view') === 'split');
	function show(file: ChangedFile | null, sideBySide: boolean) {
		const hash = new URLSearchParams();
		path = file?.newPath ?? file?.oldPath ?? null;
		split = sideBySide;
		if (path) hash.set('file', path);
		if (sideBySide) hash.set('view', 'split');
		void goto('#' + hash.toString(), { shallow: true, replace: true });
	}
	let patch = $state.raw<FileDiffNode | null>(null);
	let error = $state('');
	let retry = $state(0);
	const totals = $derived(
		files.reduce((sum, f) => ({ added: sum.added + f.stat.added, deleted: sum.deleted + f.stat.deleted }), {
			added: 0,
			deleted: 0
		})
	);
	$effect(() => {
		void retry;
		patch = null;
		error = '';
		if (!selected) return;
		const { oldPath, newPath } = selected;
		const controller = new AbortController();
		graphql<CompareFileQuery, CompareFileQueryVariables>(
			COMPARE_FILE,
			{ repo, base, head, oldPath, newPath },
			{ signal: controller.signal }
		)
			.then((data) => {
				if (controller.signal.aborted) return;
				patch = data.repository?.commit?.diff.fileDiff ?? null;
				if (!patch) error = 'This change could not be found.';
			})
			.catch((err) => {
				if (!controller.signal.aborted) error = errorMessage(err);
			});
		return () => controller.abort();
	});
</script>

<div class="toolbar">
	<h3>{files.length.toLocaleString()}{truncated ? '+' : ''} {files.length === 1 ? 'file' : 'files'} changed</h3>
	<span class="added">+{totals.added.toLocaleString()}</span>
	<span class="deleted">−{totals.deleted.toLocaleString()}</span>
	<div class="modes" role="group" aria-label="Diff layout">
		<button class="btn btn-sm" aria-pressed={!split} onclick={() => show(selected, false)}>Unified</button>
		<button class="btn btn-sm" aria-pressed={split} onclick={() => show(selected, true)}>Side by side</button>
	</div>
</div>
{#if truncated}<p class="alert alert-warning">
		Showing the first {files.length.toLocaleString()} changed files. Totals cover these files.
	</p>{/if}
<div class="review">
	<ChangedFileTree {files} {selected} onselect={(f) => show(f, split)} />
	<div class="patch">
		{#if error}
			<p class="alert alert-danger" role="alert">
				{error} <button class="btn btn-sm" onclick={() => retry++}>Retry</button>
			</p>
		{:else if patch}
			<FilePatch {repo} f={patch} headRev={head} {split} />
		{:else}
			<p class="muted" role="status">Loading patch…</p>
		{/if}
	</div>
</div>

<style>
	.toolbar {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		margin: var(--space-4) 0 var(--space-2);
		flex-wrap: wrap;
	}
	h3 {
		font-size: 1rem;
		margin: 0;
	}
	.added,
	.deleted {
		font-size: var(--text-sm);
		font-variant-numeric: tabular-nums;
	}
	.added {
		color: var(--diff-added-text);
	}
	.deleted {
		color: var(--diff-deleted-text);
	}
	.modes {
		margin-left: auto;
		display: flex;
	}
	.modes button:first-child {
		border-radius: 0.35rem 0 0 0.35rem;
	}
	.modes button:last-child {
		border-radius: 0 0.35rem 0.35rem 0;
		margin-left: -1px;
	}
	button[aria-pressed='true'] {
		background: var(--color-info-bg);
		color: var(--color-link);
	}
	.review {
		display: grid;
		grid-template-columns: minmax(12rem, 17rem) minmax(0, 1fr);
		gap: var(--space-3);
	}
	@media (max-width: 800px) {
		.review {
			grid-template-columns: minmax(0, 1fr);
		}
	}
</style>
