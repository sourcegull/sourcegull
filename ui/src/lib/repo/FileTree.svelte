<script lang="ts">
	import Icon from '#lib/components/Icon.svelte';
	import { SvelteMap, SvelteSet } from 'svelte/reactivity';
	import { errorMessage } from '#lib/graphql.ts';
	import { loadTree, type TreeEntryNode } from './queries.ts';
	import { ancestorPaths, repoPageURL } from './url.ts';

	interface Props {
		repo: string;
		/** Revision in URLs (null = default branch). */
		urlRev: string | null;
		/** Path of the open file or directory. */
		activePath: string;
		activeIsDir: boolean;
	}

	let { repo, urlRev, activePath, activeIsDir }: Props = $props();

	/** Row height in px; rows are positioned absolutely for virtualization. */
	const ROW = 24;
	const OVERSCAN = 12;

	type DirState =
		| { status: 'loading' }
		| { status: 'error'; message: string }
		| { status: 'loaded'; entries: TreeEntryNode[] };

	const dirs = new SvelteMap<string, DirState>();
	const expanded = new SvelteSet<string>();
	let scrollTop = $state(0);
	let viewportHeight = $state(600);
	let scroller: HTMLDivElement | undefined = $state();

	const queryRev = $derived(urlRev ?? 'HEAD');

	async function load(path: string): Promise<void> {
		const current = dirs.get(path);
		if (current && current.status !== 'error') return;
		const key = `${repo}@${queryRev}`;
		dirs.set(path, { status: 'loading' });
		try {
			const r = await loadTree(repo, queryRev, path);
			if (key !== `${repo}@${queryRev}`) return;
			const entries = r.repository?.commit?.tree?.entries ?? [];
			dirs.set(path, { status: 'loaded', entries });
		} catch (err) {
			dirs.set(path, { status: 'error', message: errorMessage(err) });
		}
	}

	// Reset when the repository or revision changes.
	let loadedFor = '';
	$effect(() => {
		const key = `${repo}@${queryRev}`;
		if (key === loadedFor) return;
		loadedFor = key;
		dirs.clear();
		expanded.clear();
		void load('');
	});

	// Reveal the active path: expand its ancestors (and itself for directories).
	$effect(() => {
		const toOpen = ancestorPaths(activePath);
		if (activeIsDir && activePath !== '') toOpen.push(activePath);
		for (const p of toOpen) {
			expanded.add(p);
			void load(p);
		}
	});

	interface Row {
		key: string;
		depth: number;
		entry: TreeEntryNode | null;
		/** Placeholder text for loading/error rows. */
		note?: string;
	}

	const rows = $derived.by(() => {
		const out: Row[] = [];
		const walk = (path: string, depth: number) => {
			const d = dirs.get(path);
			if (!d || d.status === 'loading') {
				out.push({ key: `${path}/…loading`, depth, entry: null, note: 'Loading…' });
				return;
			}
			if (d.status === 'error') {
				out.push({ key: `${path}/…error`, depth, entry: null, note: d.message });
				return;
			}
			for (const e of d.entries) {
				out.push({ key: e.path, depth, entry: e });
				if (e.isDirectory && expanded.has(e.path)) walk(e.path, depth + 1);
			}
		};
		walk('', 0);
		return out;
	});

	const first = $derived(Math.max(0, Math.floor(scrollTop / ROW) - OVERSCAN));
	const last = $derived(Math.min(rows.length, Math.ceil((scrollTop + viewportHeight) / ROW) + OVERSCAN));
	const visible = $derived(rows.slice(first, last).map((row, i) => ({ row, index: first + i })));

	// Keep the active entry in view when it changes.
	$effect(() => {
		const i = rows.findIndex((r) => r.entry?.path === activePath);
		const el = scroller;
		if (i < 0 || !el) return;
		const top = i * ROW;
		if (top < el.scrollTop || top + ROW > el.scrollTop + el.clientHeight) {
			el.scrollTop = Math.max(0, top - el.clientHeight / 3);
		}
	});

	function toggle(e: TreeEntryNode): void {
		if (expanded.has(e.path)) {
			expanded.delete(e.path);
		} else {
			expanded.add(e.path);
			void load(e.path);
		}
	}

	function href(e: TreeEntryNode): string {
		return repoPageURL(repo, urlRev, e.isDirectory ? 'tree' : 'blob', e.path);
	}
</script>

<nav class="file-tree" aria-label="Files">
	<div
		class="scroller"
		bind:this={scroller}
		bind:clientHeight={viewportHeight}
		onscroll={(ev) => (scrollTop = (ev.currentTarget as HTMLDivElement).scrollTop)}
	>
		<ul class="rows" role="tree" style:height="{rows.length * ROW}px">
			{#each visible as { row, index } (row.key)}
				<li
					class="row"
					role="treeitem"
					aria-selected={row.entry?.path === activePath}
					aria-expanded={row.entry?.isDirectory ? expanded.has(row.entry.path) : undefined}
					style:top="{index * ROW}px"
					style:padding-left="{0.5 + row.depth * 0.9}rem"
				>
					{#if row.entry}
						{@const e = row.entry}
						{#if e.isDirectory}
							<button
								type="button"
								class="chevron"
								class:open={expanded.has(e.path)}
								aria-label={expanded.has(e.path) ? `Collapse ${e.name}` : `Expand ${e.name}`}
								onclick={() => toggle(e)}><Icon name="chevron-right" size={14} /></button
							>
						{:else}
							<span class="chevron-space" aria-hidden="true"></span>
						{/if}
						<a
							href={href(e)}
							class:active={e.path === activePath}
							class:dir={e.isDirectory}
							title={e.path}
							onclick={() => {
								if (e.isDirectory && !expanded.has(e.path)) toggle(e);
							}}
						>
							<span class="entry-icon"><Icon name={e.isDirectory ? 'folder' : 'file'} size={14} /></span>{e.name}{#if e.isDirectory}<span class="visually-hidden">/</span>{:else if e.kind === 'SUBMODULE'}<span
									class="muted"> (submodule)</span
								>{/if}
						</a>
					{:else}
						<span class="note muted">{row.note}</span>
					{/if}
				</li>
			{/each}
		</ul>
	</div>
</nav>

<style>
	.file-tree {
		height: 100%;
		display: flex;
		flex-direction: column;
		min-height: 0;
	}
	.scroller {
		flex: 1;
		overflow: auto;
		min-height: 0;
	}
	.rows {
		position: relative;
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.row {
		position: absolute;
		left: 0;
		right: 0;
		height: 24px;
		display: flex;
		align-items: center;
		gap: 0.15rem;
		white-space: nowrap;
		font-size: 0.92em;
	}
	.row a {
		flex: 1;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		color: var(--color-text);
		padding: 0 0.4rem;
		border-radius: var(--radius-sm);
		line-height: 22px;
		font-size: var(--text-sm);
	}
	.row a:hover {
		background: var(--color-bg-hover);
		text-decoration: none;
	}
	.row a.active {
		background: var(--color-primary-subtle);
		color: var(--color-primary);
		font-weight: var(--weight-medium);
	}
	.entry-icon {
		display: inline-flex;
		margin-right: 0.35rem;
		vertical-align: -2px;
		color: var(--color-text-subtle);
	}
	.dir .entry-icon {
		color: var(--color-primary);
		opacity: 0.8;
	}
	.active .entry-icon {
		color: currentColor;
	}
	.chevron,
	.chevron-space {
		flex: 0 0 1rem;
		width: 1rem;
		text-align: center;
	}
	.chevron {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		height: 20px;
		border: none;
		border-radius: var(--radius-sm);
		background: none;
		padding: 0;
		color: var(--color-text-subtle);
		cursor: pointer;
	}
	.chevron:hover {
		color: var(--color-text);
		background: var(--color-bg-hover);
	}
	.chevron :global(.icon) {
		transition: transform var(--duration) var(--ease);
	}
	.chevron.open :global(.icon) {
		transform: rotate(90deg);
	}
	.note {
		padding-left: 1.2rem;
		overflow: hidden;
		text-overflow: ellipsis;
	}
</style>
