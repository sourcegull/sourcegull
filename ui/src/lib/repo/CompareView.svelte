<script lang="ts">
	import { goto } from '$app/navigation';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import CommitTable from './CommitTable.svelte';
	import DiffView from './DiffView.svelte';
	import { cloneStateMessage, COMPARE, type CompareResult } from './queries.ts';
	import { parseCompareSpec, repoPageURL } from './url.ts';

	interface Props {
		repo: string;
		/** `base...head` from the URL (may be empty). */
		spec: string;
		defaultBranch: string | null;
	}

	let { repo, spec, defaultBranch }: Props = $props();

	const parsed = $derived(parseCompareSpec(spec));
	let base = $state('');
	let head = $state('');
	let result = $state.raw<NonNullable<CompareResult['repository']>['comparison'] | null>(null);
	let loading = $state(false);
	let error = $state<string | null>(null);
	let seq = 0;

	$effect(() => {
		base = parsed.base || defaultBranch || '';
		head = parsed.head;
		result = null;
		if (spec) void load(repo, parsed.base, parsed.head);
	});

	async function load(repoName: string, b: string, h: string): Promise<void> {
		const mine = ++seq;
		loading = true;
		error = null;
		try {
			const r = await graphql<CompareResult, { repo: string; base: string | null; head: string | null }>(
				COMPARE,
				{ repo: repoName, base: b || null, head: h || null }
			);
			if (mine !== seq) return;
			result = r.repository?.comparison ?? null;
		} catch (err) {
			if (mine === seq) error = cloneStateMessage(err) ?? errorMessage(err);
		} finally {
			if (mine === seq) loading = false;
		}
	}

	function onsubmit(event: SubmitEvent): void {
		event.preventDefault();
		if (!head.trim()) return;
		void goto(repoPageURL(repo, null, 'compare', `${base.trim()}...${head.trim()}`));
	}
</script>

<div class="compare-view">
	<h2>Compare revisions</h2>
	<form class="compare-form" {onsubmit}>
		<label class="visually-hidden" for="cmp-base">Base</label>
		<input id="cmp-base" class="input mono" placeholder="base (default branch)" bind:value={base} />
		<span class="muted">…</span>
		<label class="visually-hidden" for="cmp-head">Head</label>
		<input id="cmp-head" class="input mono" placeholder="head (branch, tag or commit)" bind:value={head} />
		<button class="btn btn-primary" type="submit">Compare</button>
	</form>

	{#if error}
		<div class="alert alert-danger" role="alert">{error}</div>
	{:else if loading}
		<p class="muted">Comparing…</p>
	{:else if result}
		<p class="muted">
			<code>{result.range.expr}</code>
			{#if result.range.mergeBase}· merge base <code>{result.range.mergeBase.abbreviatedOID}</code>{/if}
		</p>
		<h3>{result.commits.nodes.length}{result.commits.truncated ? '+' : ''} commits</h3>
		<CommitTable {repo} commits={result.commits.nodes} emptyText="The head has no commits that are not in the base." />
		<DiffView {repo} diffs={result.fileDiffs} headRev={parsed.head || 'HEAD'} />
	{:else if !spec}
		<p class="muted">Choose two revisions to see the commits and changes between them.</p>
	{/if}
</div>

<style>
	.compare-view {
		padding: var(--space-4);
		max-width: 90rem;
	}
	.compare-form {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		margin-bottom: var(--space-4);
		max-width: 48rem;
	}
	h3 {
		margin-top: var(--space-4);
		font-size: 1rem;
	}
</style>
