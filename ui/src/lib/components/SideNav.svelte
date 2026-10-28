<script lang="ts">
	import { page } from '$app/state';
	import { currentHref, type NavItem, type NavSection } from './side-nav.ts';

	interface Props {
		label: string;
		/** A flat list of links (one unlabelled section). */
		items?: NavItem[];
		/** Labelled groups of links; used instead of `items` when given. */
		sections?: NavSection[];
		id?: string;
	}

	let { label, items = [], sections, id }: Props = $props();

	const groups = $derived<NavSection[]>(sections ?? [{ items }]);
	const current = $derived(
		currentHref(
			groups.flatMap((g) => g.items),
			page.url.pathname
		)
	);
</script>

<nav class="side-nav" aria-label={label} {id}>
	{#each groups as group, i (group.title ?? i)}
		<div class="group">
			{#if group.title}
				<h3 class="group-title">{group.title}</h3>
			{/if}
			<ul>
				{#each group.items as item (item.href)}
					<li>
						<a href={item.href} aria-current={item.href === current ? 'page' : undefined}
							>{item.title}</a
						>
					</li>
				{/each}
			</ul>
		</div>
	{/each}
</nav>

<style>
	.group + .group {
		margin-top: var(--space-4);
	}
	.group-title {
		margin: 0 0 var(--space-1);
		padding: 0 var(--space-3);
		font-size: var(--text-xs);
		font-weight: var(--weight-semibold);
		letter-spacing: 0.04em;
		text-transform: uppercase;
		color: var(--color-text-subtle);
	}
	.side-nav ul {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 1px;
	}
	.side-nav a {
		display: block;
		position: relative;
		padding: 0.3125rem var(--space-3);
		border-radius: var(--radius);
		color: var(--color-text-muted);
		font-weight: var(--weight-medium);
		font-size: var(--text-sm);
		transition:
			background-color var(--duration) var(--ease),
			color var(--duration) var(--ease);
	}
	.side-nav a:hover {
		background: var(--color-bg-hover);
		color: var(--color-text);
		text-decoration: none;
	}
	.side-nav a[aria-current='page'] {
		background: var(--color-primary-subtle);
		color: var(--color-text);
	}
	.side-nav a[aria-current='page']::before {
		content: '';
		position: absolute;
		left: 0;
		top: 0.375rem;
		bottom: 0.375rem;
		width: 2px;
		border-radius: var(--radius-full);
		background: var(--color-primary);
	}
	.side-nav a:focus-visible {
		outline: none;
		box-shadow: inset 0 0 0 2px var(--color-focus);
	}
</style>
