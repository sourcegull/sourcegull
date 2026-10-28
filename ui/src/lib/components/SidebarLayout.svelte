<script lang="ts">
	import type { Snippet } from 'svelte';
	import { page } from '$app/state';
	import Icon from './Icon.svelte';
	import SideNav from './SideNav.svelte';
	import { currentHref, type NavItem, type NavSection } from './side-nav.ts';

	interface Props {
		title: string;
		/** A flat list of links. */
		items?: NavItem[];
		/** Labelled groups of links (used instead of `items` when given). */
		sections?: NavSection[];
		children: Snippet;
	}

	let { title, items = [], sections, children }: Props = $props();

	const uid = $props.id();
	const navId = `section-nav-${uid}`;
	const all = $derived((sections ?? [{ items }]).flatMap((s) => s.items));
	const currentTitle = $derived.by(() => {
		const href = currentHref(all, page.url.pathname);
		return all.find((i) => i.href === href)?.title;
	});

	/** Below 900px the navigation collapses behind a disclosure button showing the current page. */
	let open = $state(false);
	$effect(() => {
		void page.url.pathname;
		open = false;
	});

	let aside: HTMLElement | undefined = $state();
	function onKeydown(e: KeyboardEvent): void {
		if (open && e.key === 'Escape') {
			open = false;
			aside?.querySelector<HTMLElement>('.nav-toggle')?.focus();
		}
	}
	function onPointerdown(e: PointerEvent): void {
		if (open && aside && !aside.contains(e.target as Node)) open = false;
	}
</script>

<svelte:window onkeydown={onKeydown} onpointerdown={onPointerdown} />

<div class="with-sidebar">
	<aside class:open bind:this={aside}>
		<h2 class="sidebar-title">{title}</h2>
		<button
			type="button"
			class="nav-toggle"
			aria-expanded={open}
			aria-controls={navId}
			onclick={() => (open = !open)}
		>
			<span class="toggle-text">
				<span class="toggle-section">{title}</span>
				<span class="toggle-current">{currentTitle ?? 'Menu'}</span>
			</span>
			<Icon name="chevron-down" size={16} />
		</button>
		<div class="nav-panel">
			<SideNav label={title} {items} {sections} id={navId} />
		</div>
	</aside>
	<div class="content">
		{@render children()}
	</div>
</div>

<style>
	.with-sidebar {
		display: grid;
		grid-template-columns: 14rem minmax(0, 1fr);
		gap: var(--space-8);
		max-width: var(--content-max);
		margin: 0 auto;
		padding: var(--space-6) var(--space-6) var(--space-12);
	}
	aside {
		position: sticky;
		top: calc(var(--nav-height) + var(--space-6));
		align-self: start;
		max-height: calc(100vh - var(--nav-height) - 2 * var(--space-6));
		overflow-y: auto;
		scrollbar-width: thin;
	}
	.sidebar-title {
		font-size: var(--text-md);
		font-weight: var(--weight-semibold);
		color: var(--color-text);
		margin: 0 0 var(--space-3) var(--space-3);
	}
	.nav-toggle {
		display: none;
	}
	.content {
		min-width: 0;
	}
	@media (max-width: 56.25rem) {
		.with-sidebar {
			grid-template-columns: minmax(0, 1fr);
			gap: var(--space-4);
			padding: var(--space-4) var(--space-4) var(--space-8);
		}
		aside {
			position: relative;
			top: auto;
			max-height: none;
			overflow: visible;
		}
		.sidebar-title {
			display: none;
		}
		.nav-toggle {
			display: flex;
			align-items: center;
			justify-content: space-between;
			gap: var(--space-3);
			width: 100%;
			min-height: var(--control-height-lg);
			padding: var(--space-2) var(--space-3);
			border: 1px solid var(--color-border-strong);
			border-radius: var(--radius);
			background: var(--color-surface);
			color: var(--color-text);
			font: inherit;
			text-align: left;
			cursor: pointer;
			box-shadow: var(--shadow-xs);
		}
		.nav-toggle:focus-visible {
			outline: none;
			box-shadow: var(--focus-ring);
		}
		.toggle-text {
			display: flex;
			flex-direction: column;
			min-width: 0;
		}
		.toggle-section {
			font-size: var(--text-xs);
			color: var(--color-text-subtle);
		}
		.toggle-current {
			font-weight: var(--weight-medium);
			overflow: hidden;
			text-overflow: ellipsis;
			white-space: nowrap;
		}
		.nav-toggle :global(svg) {
			flex: none;
			color: var(--color-text-muted);
			transition: transform var(--duration) var(--ease);
		}
		aside.open .nav-toggle :global(svg) {
			transform: rotate(180deg);
		}
		.nav-panel {
			display: none;
		}
		aside.open .nav-panel {
			display: block;
			position: absolute;
			z-index: 20;
			top: calc(100% + var(--space-1));
			left: 0;
			right: 0;
			max-height: min(70vh, 36rem);
			overflow-y: auto;
			padding: var(--space-3) var(--space-2);
			background: var(--color-surface-raised);
			border: 1px solid var(--color-border);
			border-radius: var(--radius-lg);
			box-shadow: var(--shadow-lg);
		}
	}
</style>
