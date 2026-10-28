<script lang="ts">
	import Icon, { type IconName } from '#lib/components/Icon.svelte';
	import type { Filter } from './stream.ts';

	interface Props {
		/** The query as typed (without the context). */
		query: string;
		filters: Filter[];
		/** Removes and adds query fragments, then searches again. */
		onchange: (remove: string[], add: string[]) => void;
	}

	let { query, filters, onchange }: Props = $props();

	const TYPES: { label: string; fragment: string; icon: IconName }[] = [
		{ label: 'Code', fragment: '', icon: 'code' },
		{ label: 'Repositories', fragment: 'type:repo', icon: 'repo' },
		{ label: 'Paths', fragment: 'type:path', icon: 'file' },
		{ label: 'Symbols', fragment: 'type:symbol', icon: 'symbol' },
		{ label: 'Commits', fragment: 'type:commit', icon: 'commit' },
		{ label: 'Diffs', fragment: 'type:diff', icon: 'branch' }
	];

	const SECTIONS: { kind: string; title: string }[] = [
		{ kind: 'lang', title: 'Languages' },
		{ kind: 'repo', title: 'Repositories' },
		{ kind: 'file', title: 'File types' },
		{ kind: 'author', title: 'Authors' },
		{ kind: 'symbol type', title: 'Symbol kinds' }
	];

	const PER_SECTION = 8;

	const words = $derived(new Set(query.trim().split(/\s+/)));
	const currentType = $derived(TYPES.find((t) => t.fragment && words.has(t.fragment))?.fragment ?? '');
	let expanded = $state<Record<string, boolean>>({});

	function byKind(kind: string): Filter[] {
		return filters.filter((f) => f.kind === kind);
	}

	function selectType(fragment: string): void {
		if (fragment === currentType) return;
		onchange(currentType ? [currentType] : [], fragment ? [fragment] : []);
	}
</script>

<nav class="sidebar" aria-label="Search filters">
	<section>
		<h2>Result type</h2>
		<ul>
			{#each TYPES as t (t.label)}
				<li>
					<button
						type="button"
						class="filter"
						aria-pressed={t.fragment === currentType}
						onclick={() => selectType(t.fragment)}
					>
						<span class="label"><Icon name={t.icon} size={15} /><span class="label-text">{t.label}</span></span>
					</button>
				</li>
			{/each}
		</ul>
	</section>

	{#each SECTIONS as s (s.kind)}
		{@const items = byKind(s.kind)}
		{#if items.length > 0}
			{@const shown = expanded[s.kind] ? items : items.slice(0, PER_SECTION)}
			<section>
				<h2>{s.title}</h2>
				<ul>
					{#each shown as f (f.value)}
						{@const active = words.has(f.value)}
						<li>
							<button
								type="button"
								class="filter"
								aria-pressed={active}
								title={active ? `Remove ${f.value}` : `Add ${f.value}`}
								onclick={() => (active ? onchange([f.value], []) : onchange([], [f.value]))}
							>
								<span class="label">{#if active}<Icon name="check" size={14} />{/if}<span class="label-text">{f.label}</span></span>
								<span class="count">{f.count}{f.exhaustive ? '' : '+'}</span>
							</button>
						</li>
					{/each}
				</ul>
				{#if items.length > PER_SECTION}
					<button
						type="button"
						class="btn-link toggle"
						onclick={() => (expanded = { ...expanded, [s.kind]: !expanded[s.kind] })}
					>
						{expanded[s.kind] ? 'Show fewer' : `Show ${items.length - PER_SECTION} more`}
					</button>
				{/if}
			</section>
		{/if}
	{/each}
</nav>

<style>
	.sidebar {
		display: grid;
		grid-template-columns: minmax(0, 1fr);
		gap: var(--space-5);
		align-content: start;
		font-size: var(--text-sm);
	}
	h2 {
		font-size: var(--text-xs);
		font-weight: var(--weight-semibold);
		text-transform: uppercase;
		letter-spacing: 0.05em;
		color: var(--color-text-subtle);
		margin: 0 0 var(--space-1);
		padding: 0 var(--space-2);
	}
	ul {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		grid-template-columns: minmax(0, 1fr);
		gap: 1px;
	}
	.filter {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--space-2);
		width: 100%;
		min-height: 1.75rem;
		padding: 0.15rem var(--space-2);
		border: 0;
		border-radius: var(--radius);
		background: none;
		color: var(--color-text);
		font: inherit;
		text-align: left;
		cursor: pointer;
		transition: background-color var(--duration) var(--ease);
	}
	.filter:hover {
		background: var(--color-bg-hover);
	}
	.filter:focus-visible {
		outline: none;
		box-shadow: var(--focus-ring);
	}
	.filter[aria-pressed='true'] {
		background: var(--color-primary-subtle);
		color: var(--color-primary);
		font-weight: var(--weight-medium);
	}
	.label {
		flex: 1;
		display: inline-flex;
		align-items: center;
		gap: var(--space-2);
		min-width: 0;
	}
	.label-text {
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.label :global(.icon) {
		color: var(--color-text-subtle);
	}
	.filter[aria-pressed='true'] .label :global(.icon) {
		color: currentColor;
	}
	.count {
		flex: none;
		padding: 0 0.4rem;
		border-radius: var(--radius-full);
		background: var(--color-bg-muted);
		color: var(--color-text-muted);
		font-size: var(--text-xs);
		line-height: 1.25rem;
		font-variant-numeric: tabular-nums;
	}
	.toggle {
		font-size: var(--text-xs);
		padding: var(--space-1) var(--space-2) 0;
	}
</style>
