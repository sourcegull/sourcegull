<script lang="ts">
	// A `replace-in-place` compute result as a diff of the original file against the rewritten one.
	import DiffView from '#lib/repo/DiffView.svelte';
	import { textFileDiff } from '#lib/repo/diff.ts';
	import type { FileDiffConnection } from '#lib/repo/queries.ts';
	import { originalFileURL, type ComputeText } from './compute.ts';

	interface Props {
		result: ComputeText;
	}

	let { result }: Props = $props();

	let original = $state<string | null>(null);
	let failed = $state(false);
	let showFile = $state(false);

	$effect(() => {
		const url = originalFileURL(result);
		original = null;
		failed = false;
		if (!url) {
			failed = true;
			return;
		}
		const c = new AbortController();
		fetch(url, { credentials: 'same-origin', signal: c.signal })
			.then(async (res) => {
				if (!res.ok) throw new Error(res.statusText);
				original = await res.text();
			})
			.catch(() => {
				if (!c.signal.aborted) failed = true;
			});
		return () => c.abort();
	});

	const diffs: FileDiffConnection | null = $derived.by(() => {
		if (original == null) return null;
		const node = textFileDiff(result.path ?? '', original, result.value);
		return { totalCount: 1, diffStat: node.stat, nodes: [node] };
	});
</script>

<div class="replace">
	{#if diffs && !showFile}
		<DiffView repo={result.repository ?? ''} {diffs} headRev={result.commit} />
	{:else if !failed && !showFile}
		<p class="muted">Loading the original file…</p>
	{:else}
		{#if failed}<p class="muted">The original file could not be loaded; showing the rewritten file.</p>{/if}
		<pre>{result.value}</pre>
	{/if}
	{#if diffs}
		<button type="button" class="btn btn-sm btn-ghost" onclick={() => (showFile = !showFile)}>
			{showFile ? 'Show diff' : 'Show rewritten file'}
		</button>
	{/if}
</div>

<style>
	pre {
		margin: 0;
		white-space: pre-wrap;
		overflow-wrap: anywhere;
	}
</style>
