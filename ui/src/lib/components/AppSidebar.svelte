<script lang="ts">
	import { page } from '$app/state';
	import Icon from './Icon.svelte';
	import Wordmark from './Wordmark.svelte';
	import { isItemActive, type NavGroup } from './nav.ts';

	interface Props {
		groups: NavGroup[];
		/** Desktop: icon rail instead of the full sidebar. Ignored in drawer mode. */
		collapsed: boolean;
		/** Narrow screens: the sidebar is an off-canvas drawer. */
		drawer: boolean;
		/** Drawer mode: whether the drawer is shown. */
		open: boolean;
		ontogglecollapsed: () => void;
		onclose: () => void;
	}

	let { groups, collapsed, drawer, open, ontogglecollapsed, onclose }: Props = $props();

	const rail = $derived(collapsed && !drawer);
	let root: HTMLElement;

	// Opening the drawer moves focus into it, so keyboard users land in the navigation.
	$effect(() => {
		if (drawer && open) root?.querySelector<HTMLElement>('a[aria-current="page"], nav a')?.focus();
	});

	function onkeydown(event: KeyboardEvent) {
		if (drawer && open && event.key === 'Escape') {
			event.stopPropagation();
			onclose();
		}
	}
</script>

<!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
<aside
	id="app-sidebar"
	class="sidebar"
	class:rail
	class:drawer
	class:open
	aria-label="Sidebar"
	inert={drawer && !open}
	bind:this={root}
	{onkeydown}
>
	<div class="sidebar-header">
		<a class="brand" href="/search" title="Sourcegull">
			<img class="logo" src="/logo.png" alt="" width="41" height="30" />
			<span class="brand-name label-text"><Wordmark /></span>
		</a>
		{#if drawer}
			<button class="btn btn-ghost btn-icon btn-sm" type="button" aria-label="Close navigation" onclick={onclose}>
				<Icon name="close" />
			</button>
		{/if}
	</div>

	<nav aria-label="Main">
		{#each groups as group, gi (group.label ?? gi)}
			<div class="group">
				{#if group.label}
					<div class="group-label label-text" id="nav-group-{gi}">{group.label}</div>
				{/if}
				<ul aria-labelledby={group.label ? `nav-group-${gi}` : undefined}>
					{#each group.items as item (item.href)}
						{@const current = isItemActive(item, page.url.pathname)}
						<li>
							<a
								href={item.href}
								aria-current={current ? 'page' : undefined}
								title={rail ? item.label : undefined}
								onclick={() => drawer && onclose()}
							>
								<Icon name={item.icon} size={17} />
								<span class="label-text">{item.label}</span>
							</a>
						</li>
					{/each}
				</ul>
			</div>
		{/each}
	</nav>

	{#if !drawer}
		<div class="sidebar-footer">
			<button
				class="collapse-toggle"
				type="button"
				aria-pressed={collapsed}
				title={collapsed ? 'Expand sidebar' : 'Collapse sidebar'}
				onclick={ontogglecollapsed}
			>
				<Icon name={collapsed ? 'chevron-right' : 'chevron-left'} size={17} />
				<span class="label-text">Collapse sidebar</span>
			</button>
		</div>
	{/if}
</aside>

<style>
	.sidebar {
		position: sticky;
		top: 0;
		height: 100vh;
		height: 100dvh;
		width: var(--sidebar-width);
		flex: none;
		display: flex;
		flex-direction: column;
		background: var(--color-sidebar);
		border-right: 1px solid var(--color-border);
		transition: width var(--duration) var(--ease);
		z-index: 30;
	}
	.sidebar.rail {
		width: var(--sidebar-rail-width);
	}

	.sidebar-header {
		display: flex;
		align-items: center;
		justify-content: space-between;
		height: calc(var(--nav-height) + var(--space-2));
		padding: var(--space-2) var(--space-4) 0;
		flex: none;
	}
	.brand {
		display: inline-flex;
		align-items: center;
		gap: var(--space-2);
		padding: var(--space-1);
		margin-left: calc(-1 * var(--space-1));
		border-radius: var(--radius);
		color: var(--color-text);
		font-weight: var(--weight-semibold);
		font-size: var(--text-xl);
		letter-spacing: -0.02em;
	}
	.brand:hover {
		text-decoration: none;
	}
	.logo {
		flex: none;
		display: block;
	}

	nav {
		flex: 1;
		overflow-y: auto;
		padding: var(--space-3) var(--space-2) var(--space-4);
	}
	.group + .group {
		margin-top: var(--space-4);
	}
	.group-label {
		padding: 0 var(--space-2) var(--space-1);
		color: var(--color-text-subtle);
		font-size: var(--text-xs);
		font-weight: var(--weight-medium);
		text-transform: uppercase;
		letter-spacing: 0.06em;
	}
	ul {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: 1px;
	}
	nav a,
	.collapse-toggle {
		position: relative;
		display: flex;
		align-items: center;
		gap: var(--space-3);
		width: 100%;
		height: 2rem;
		padding: 0 var(--space-2);
		border: none;
		border-radius: var(--radius);
		background: none;
		color: var(--color-text-muted);
		font: inherit;
		font-weight: var(--weight-medium);
		white-space: nowrap;
		cursor: pointer;
		transition:
			background-color var(--duration) var(--ease),
			color var(--duration) var(--ease);
	}
	nav a:hover,
	.collapse-toggle:hover {
		background: var(--color-bg-hover);
		color: var(--color-text);
		text-decoration: none;
	}
	nav a[aria-current='page'] {
		background: var(--color-primary-subtle);
		color: var(--color-info);
	}
	nav a[aria-current='page']::before {
		content: '';
		position: absolute;
		left: calc(-1 * var(--space-2));
		top: 0.4rem;
		bottom: 0.4rem;
		width: 3px;
		border-radius: 0 3px 3px 0;
		background: var(--color-primary);
	}
	nav a:focus-visible,
	.collapse-toggle:focus-visible {
		outline: none;
		box-shadow: inset 0 0 0 2px var(--color-focus);
	}

	.sidebar-footer {
		flex: none;
		padding: var(--space-2);
		border-top: 1px solid var(--color-border);
	}

	/* Icon rail: labels become visually hidden (still read by screen readers). */
	.rail .label-text {
		position: absolute;
		width: 1px;
		height: 1px;
		overflow: hidden;
		clip: rect(0, 0, 0, 0);
		white-space: nowrap;
	}
	.rail .sidebar-header {
		justify-content: center;
		padding: 0;
	}
	.rail .brand {
		margin: 0;
	}
	.rail .logo {
		width: 38px;
		height: auto;
	}
	.rail nav a,
	.rail .collapse-toggle {
		justify-content: center;
		padding: 0;
	}
	.rail .group + .group {
		margin-top: var(--space-2);
		padding-top: var(--space-2);
		border-top: 1px solid var(--color-border);
	}

	/* Drawer (narrow screens): off-canvas, slides in over the content. */
	.sidebar.drawer {
		position: fixed;
		left: 0;
		top: 0;
		bottom: 0;
		width: min(17rem, 85vw);
		transform: translateX(-100%);
		visibility: hidden;
		box-shadow: none;
		transition:
			transform 200ms var(--ease),
			visibility 0s linear 200ms;
	}
	.sidebar.drawer.open {
		transform: none;
		visibility: visible;
		box-shadow: var(--shadow-lg);
		transition:
			transform 200ms var(--ease),
			visibility 0s;
	}
</style>
