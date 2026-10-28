<script lang="ts">
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		SEARCH_CONTEXTS,
		type SearchContextNode,
		type SearchContextsResult,
		type SearchContextsVariables
	} from './queries.ts';

	interface Props {
		/** Selected context spec, null for none. */
		value: string | null;
		onselect: (spec: string) => void;
	}

	let { value, onselect }: Props = $props();

	let open = $state(false);
	let filter = $state('');
	let contexts = $state.raw<SearchContextNode[]>([]);
	let loading = $state(false);
	let error = $state<string | null>(null);
	let active = $state(0);
	let root: HTMLDivElement;
	let filterInput = $state<HTMLInputElement>();
	let seq = 0;

	async function load(): Promise<void> {
		const mine = ++seq;
		loading = true;
		error = null;
		try {
			const r = await graphql<SearchContextsResult, SearchContextsVariables>(SEARCH_CONTEXTS, {
				first: 50,
				query: filter.trim() || null
			});
			if (mine !== seq) return;
			contexts = r.searchContexts.nodes;
			active = 0;
		} catch (err) {
			if (mine === seq) error = errorMessage(err);
		} finally {
			if (mine === seq) loading = false;
		}
	}

	function toggle(): void {
		open = !open;
		if (open) {
			filter = '';
			void load();
			queueMicrotask(() => filterInput?.focus());
		}
	}

	function choose(spec: string): void {
		open = false;
		onselect(spec);
	}

	let debounce: ReturnType<typeof setTimeout> | undefined;
	function oninput(): void {
		clearTimeout(debounce);
		debounce = setTimeout(load, 150);
	}

	function onkeydown(e: KeyboardEvent): void {
		if (e.key === 'Escape') {
			open = false;
		} else if (e.key === 'ArrowDown') {
			e.preventDefault();
			active = Math.min(active + 1, contexts.length - 1);
		} else if (e.key === 'ArrowUp') {
			e.preventDefault();
			active = Math.max(active - 1, 0);
		} else if (e.key === 'Enter') {
			e.preventDefault();
			const c = contexts[active];
			if (c) choose(c.spec);
		}
	}

	function onwindowclick(e: MouseEvent): void {
		if (open && !root.contains(e.target as Node)) open = false;
	}
</script>

<svelte:window onclick={onwindowclick} />

<div class="picker" bind:this={root}>
	<button
		type="button"
		class="btn trigger mono"
		aria-haspopup="listbox"
		aria-expanded={open}
		title="Search context"
		onclick={toggle}
	>
		<span class="ctx-field">context:</span><span class="ctx-spec">{value ?? 'global'}</span>
		<span aria-hidden="true" class="caret">▾</span>
	</button>
	{#if open}
		<div class="popover card" role="dialog" aria-label="Choose a search context">
			<label class="visually-hidden" for="context-filter">Find a context</label>
			<input
				id="context-filter"
				class="input"
				type="search"
				placeholder="Find a context…"
				autocomplete="off"
				bind:this={filterInput}
				bind:value={filter}
				{oninput}
				{onkeydown}
			/>
			{#if error}
				<div class="alert alert-danger" role="alert">{error}</div>
			{/if}
			<ul role="listbox" aria-busy={loading} aria-label="Search contexts">
				{#each contexts as c, i (c.id)}
					<li
						role="option"
						aria-selected={c.spec === value}
						class:active={i === active}
						onmouseenter={() => (active = i)}
					>
						<button type="button" onclick={() => choose(c.spec)}>
							<span class="mono">{c.spec}</span>
							{#if c.viewerHasAsDefault}<span class="badge badge-primary">default</span>{/if}
							{#if c.viewerHasStarred}<span class="star" title="Starred">★</span>{/if}
							{#if c.description}<span class="muted desc">{c.description}</span>{/if}
						</button>
					</li>
				{:else}
					<li class="muted empty">{loading ? 'Loading…' : 'No contexts match.'}</li>
				{/each}
			</ul>
			<div class="footer">
				<a href="/contexts" onclick={() => (open = false)}>Manage contexts</a>
			</div>
		</div>
	{/if}
</div>

<style>
	.picker {
		position: relative;
		flex: none;
	}
	/* Stretched to the search bar's height (its items stretch); label and value share one line
	   box, so they sit on the same baseline. The class names avoid the global `.field` (form
	   field: a column with a bottom margin, which lifted the label). */
	.trigger {
		height: 100%;
		gap: 0;
		align-items: center;
		font-size: 0.9rem;
		line-height: 1.5;
		white-space: nowrap;
	}
	.ctx-field {
		color: var(--syntax-property);
	}
	.ctx-spec {
		max-width: 14rem;
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.caret {
		margin-left: var(--space-1);
		color: var(--color-text-muted);
	}
	.popover {
		position: absolute;
		top: calc(100% + var(--space-1));
		left: 0;
		z-index: 20;
		width: min(26rem, 90vw);
		padding: var(--space-2);
		box-shadow: 0 8px 24px rgb(0 0 0 / 0.15);
	}
	ul {
		list-style: none;
		margin: var(--space-2) 0 0;
		padding: 0;
		max-height: 18rem;
		overflow-y: auto;
	}
	li button {
		display: flex;
		flex-wrap: wrap;
		align-items: baseline;
		gap: var(--space-2);
		width: 100%;
		padding: var(--space-1) var(--space-2);
		border: 0;
		border-radius: var(--radius);
		background: none;
		color: var(--color-text);
		text-align: left;
		cursor: pointer;
		font: inherit;
	}
	li.active button {
		background: var(--color-bg-muted);
	}
	li[aria-selected='true'] .mono {
		font-weight: 700;
	}
	.desc {
		flex-basis: 100%;
		font-size: 0.85rem;
	}
	.star {
		color: var(--color-warning);
	}
	.empty {
		padding: var(--space-2);
	}
	.footer {
		border-top: 1px solid var(--color-border);
		margin-top: var(--space-2);
		padding-top: var(--space-2);
		font-size: 0.9rem;
	}
</style>
