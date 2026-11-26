<script lang="ts">
	import Icon from '#lib/components/Icon.svelte';
	import type { ChangedFile } from './queries.ts';

	let {
		files,
		selected,
		onselect
	}: { files: ChangedFile[]; selected: ChangedFile | null; onselect: (f: ChangedFile) => void } = $props();
	let filter = $state('');
	interface Folder {
		path: string;
		dirs: Map<string, Folder>;
		files: ChangedFile[];
	}
	function folder(path: string): Folder {
		return { path, dirs: new Map(), files: [] };
	}
	const root = $derived.by(() => {
		const root = folder('');
		const needle = filter.trim().toLowerCase();
		for (const f of files) {
			if (needle && ![f.oldPath, f.newPath].some((p) => p?.toLowerCase().includes(needle))) continue;
			const path = f.newPath ?? f.oldPath ?? '';
			let dir = root;
			for (const name of path.split('/').slice(0, -1)) {
				let child = dir.dirs.get(name);
				if (!child) {
					child = folder(dir.path + name + '/');
					dir.dirs.set(name, child);
				}
				dir = child;
			}
			dir.files.push(f);
		}
		return root;
	});
	const activePath = $derived(selected?.newPath ?? selected?.oldPath ?? '');
</script>

{#snippet branch(dir: Folder)}
	{#each [...dir.dirs] as [name, child] (name)}
		<details open={!!filter || activePath.startsWith(child.path)}>
			<summary><Icon name="folder" size={15} /> {name}</summary>
			<div class="children">{@render branch(child)}</div>
		</details>
	{/each}
	{#each dir.files as f (f)}
		{@const path = f.newPath ?? f.oldPath ?? ''}
		<button
			class:active={f === selected}
			aria-current={f === selected ? 'true' : undefined}
			title={f.oldPath && f.oldPath !== path ? f.oldPath + ' → ' + path : path}
			onclick={() => onselect(f)}
		>
			<Icon name="file" size={14} /><span class="filename">{path.slice(dir.path.length)}</span>
			<span
				class="status"
				class:added={f.status === 'ADDED'}
				class:deleted={f.status === 'DELETED'}
				aria-label={f.status.toLowerCase()}>{f.status[0]}</span
			>
		</button>
	{/each}
{/snippet}

<nav aria-label="Changed files" class="card">
	<div class="tree-head">
		<input
			class="input"
			type="search"
			placeholder="Filter files…"
			aria-label="Filter changed files"
			bind:value={filter}
		/>
	</div>
	<div class="entries">
		{@render branch(root)}
		{#if !root.dirs.size && !root.files.length}<p class="muted">No matching files.</p>{/if}
	</div>
</nav>

<style>
	nav {
		padding: 0;
		min-width: 0;
		align-self: start;
		position: sticky;
		top: var(--space-2);
		overflow: hidden;
	}
	.tree-head {
		padding: var(--space-3);
		background: var(--color-bg-subtle);
		border-bottom: 1px solid var(--color-border);
	}
	.status {
		color: var(--color-text-muted);
		font-variant-numeric: tabular-nums;
	}
	.entries {
		padding: var(--space-2);
		max-height: 61vh;
		overflow: auto;
		font-size: var(--text-sm);
	}
	summary {
		cursor: pointer;
		padding: 0.25rem;
		white-space: nowrap;
	}
	summary :global(svg) {
		vertical-align: -0.15em;
		color: var(--color-text-muted);
	}
	.children {
		margin-left: 0.65rem;
		padding-left: 0.35rem;
		border-left: 1px solid var(--color-border);
	}
	button {
		width: 100%;
		display: flex;
		align-items: center;
		gap: 0.35rem;
		text-align: left;
		padding: 0.3rem 0.45rem;
		background: transparent;
		border: none;
		border-radius: 0.25rem;
		color: inherit;
		cursor: pointer;
	}
	button:hover,
	summary:hover {
		background: var(--color-bg-subtle);
	}
	button.active {
		background: var(--color-info-bg);
		color: var(--color-link);
	}
	.filename {
		overflow: hidden;
		white-space: nowrap;
		text-overflow: ellipsis;
	}
	button :global(svg) {
		flex-shrink: 0;
	}
	.status {
		margin-left: auto;
		font: var(--text-xs) var(--font-mono);
	}
	.added {
		color: var(--diff-added-text);
	}
	.deleted {
		color: var(--diff-deleted-text);
	}
	@media (max-width: 800px) {
		nav {
			position: static;
		}
		.entries {
			max-height: 13rem;
		}
	}
</style>
