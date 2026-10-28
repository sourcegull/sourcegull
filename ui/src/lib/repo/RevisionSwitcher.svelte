<script lang="ts">
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import { GIT_REFS, type GitRefNode, type GitRefsResult, type GitRefsVariables } from './queries.ts';
	import Icon from '#lib/components/Icon.svelte';
	import { shortRev } from './url.ts';

	interface Props {
		repo: string;
		/** Revision in the URL (null = default branch). */
		current: string | null;
		defaultBranch: string | null;
		onselect: (rev: string | null) => void;
	}

	let { repo, current, defaultBranch, onselect }: Props = $props();

	let open = $state(false);
	let query = $state('');
	let refs = $state.raw<GitRefNode[]>([]);
	let loading = $state(false);
	let error = $state<string | null>(null);
	let root: HTMLDivElement | undefined = $state();
	let input: HTMLInputElement | undefined = $state();
	let seq = 0;

	const label = $derived(current ? shortRev(current) : (defaultBranch ?? 'HEAD'));
	const branches = $derived(refs.filter((r) => r.type === 'GIT_BRANCH'));
	const tags = $derived(refs.filter((r) => r.type === 'GIT_TAG'));

	async function load(q: string): Promise<void> {
		const mine = ++seq;
		loading = true;
		error = null;
		try {
			const r = await graphql<GitRefsResult, GitRefsVariables>(GIT_REFS, {
				repo,
				type: null,
				first: 100,
				query: q.trim() || null,
				order: 'AUTHORED_OR_COMMITTED_AT'
			});
			if (mine === seq) refs = r.repository?.gitRefs.nodes ?? [];
		} catch (err) {
			if (mine === seq) error = errorMessage(err);
		} finally {
			if (mine === seq) loading = false;
		}
	}

	function toggle(): void {
		open = !open;
		if (open) {
			query = '';
			void load('');
			queueMicrotask(() => input?.focus());
		}
	}

	function choose(rev: string | null): void {
		open = false;
		onselect(rev);
	}

	let debounce: ReturnType<typeof setTimeout> | undefined;
	function oninput(): void {
		clearTimeout(debounce);
		debounce = setTimeout(() => load(query), 150);
	}

	function onkeydown(event: KeyboardEvent): void {
		if (event.key === 'Escape') open = false;
		if (event.key === 'Enter' && query.trim()) {
			event.preventDefault();
			choose(query.trim());
		}
	}

	function onwindowclick(event: MouseEvent): void {
		if (open && root && !root.contains(event.target as Node)) open = false;
	}
</script>

<svelte:window onclick={onwindowclick} />

<div class="rev-switcher" bind:this={root}>
	<button type="button" class="btn btn-sm mono rev-trigger" aria-haspopup="listbox" aria-expanded={open} onclick={toggle} title="Switch branch or tag">
		<span class="rev-icon"><Icon name="branch" size={14} /></span>
		<span class="visually-hidden">rev:</span>
		<span class="rev-label">{label}</span>
		<span class="rev-caret"><Icon name="chevron-down" size={14} /></span>
	</button>
	{#if open}
		<div class="popover card" role="dialog" aria-label="Switch revision">
			<input
				bind:this={input}
				class="input"
				type="search"
				placeholder="Branch, tag or commit…"
				aria-label="Find a revision"
				bind:value={query}
				{oninput}
				{onkeydown}
			/>
			{#if error}<p class="alert alert-danger">{error}</p>{/if}
			<div class="lists" role="listbox" aria-label="Revisions">
				{#if current}
					<button type="button" class="item" role="option" aria-selected="false" onclick={() => choose(null)}>
						Default branch{defaultBranch ? ` (${defaultBranch})` : ''}
					</button>
				{/if}
				{#if branches.length > 0}
					<div class="group muted">Branches</div>
					{#each branches as r (r.name)}
						<button
							type="button"
							class="item mono"
							role="option"
							aria-selected={r.displayName === current}
							onclick={() => choose(r.displayName === defaultBranch && !current ? null : r.displayName)}
						>
							{r.displayName}
						</button>
					{/each}
				{/if}
				{#if tags.length > 0}
					<div class="group muted">Tags</div>
					{#each tags as r (r.name)}
						<button
							type="button"
							class="item mono"
							role="option"
							aria-selected={r.displayName === current}
							onclick={() => choose(r.displayName)}
						>
							{r.displayName}
						</button>
					{/each}
				{/if}
				{#if !loading && refs.length === 0}
					<p class="muted empty">
						{query.trim() ? 'No matching refs. Press Enter to use it as a revision.' : 'No refs.'}
					</p>
				{/if}
				{#if loading}<p class="muted empty">Loading…</p>{/if}
			</div>
		</div>
	{/if}
</div>

<style>
	.rev-switcher {
		position: relative;
		min-width: 0;
	}
	.rev-trigger {
		gap: var(--space-1);
		max-width: 16rem;
	}
	.rev-label {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.rev-icon,
	.rev-caret {
		display: inline-flex;
		color: var(--color-text-muted);
	}
	.popover {
		position: absolute;
		top: calc(100% + 4px);
		left: 0;
		z-index: 20;
		width: 22rem;
		padding: var(--space-2);
		box-shadow: var(--shadow-lg);
	}
	.lists {
		max-height: 22rem;
		overflow: auto;
		margin-top: var(--space-2);
	}
	.group {
		font-size: 0.8em;
		font-weight: 600;
		text-transform: uppercase;
		padding: var(--space-2) var(--space-2) var(--space-1);
	}
	.item {
		display: block;
		width: 100%;
		text-align: left;
		border: none;
		background: none;
		color: var(--color-text);
		padding: 0.25rem var(--space-2);
		border-radius: 4px;
		cursor: pointer;
		font: inherit;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.item:hover,
	.item[aria-selected='true'] {
		background: var(--color-bg-muted);
	}
	.empty {
		padding: var(--space-2);
		margin: 0;
	}
</style>
