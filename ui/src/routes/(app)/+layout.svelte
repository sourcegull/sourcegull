<script lang="ts">
	import { onMount, tick } from 'svelte';
	import { afterNavigate, goto } from '$app/navigation';
	import { page } from '$app/state';
	import { resetAuthState, signOut } from '#lib/auth.ts';
	import { errorMessage } from '#lib/graphql.ts';
	import AppSidebar from '#lib/components/AppSidebar.svelte';
	import CodeHostSignInBanner from '#lib/components/CodeHostSignInBanner.svelte';
	import Icon from '#lib/components/Icon.svelte';
	import { mainNav } from '#lib/components/nav.ts';
	import StatusMessagesNavItem from '#lib/components/StatusMessagesNavItem.svelte';
	import UserMenu from '#lib/components/UserMenu.svelte';
	import Wordmark from '#lib/components/Wordmark.svelte';
	import { loadSidebarCollapsed, storeSidebarCollapsed } from '#lib/theme/theme.ts';
	import type { LayoutProps } from './$types';

	let { data, children }: LayoutProps = $props();

	let signOutError = $state<string | null>(null);

	async function onsignout() {
		signOutError = null;
		try {
			await signOut();
			resetAuthState();
			await goto('/sign-in', { replaceState: true });
		} catch (err) {
			signOutError = errorMessage(err);
		}
	}

	const groups = $derived(
		mainNav({
			signedIn: !!data.auth.user,
			siteAdmin: !!data.auth.user?.siteAdmin,
			batchChangesEnabled: !!data.auth.batchChangesEnabled
		})
	);

	/** Below this width the sidebar is a drawer opened from the top bar (900px; see the CSS too). */
	const DRAWER_QUERY = '(max-width: 56.25rem)';

	let collapsed = $state(loadSidebarCollapsed());
	let drawer = $state(typeof window !== 'undefined' && window.matchMedia(DRAWER_QUERY).matches);
	let drawerOpen = $state(false);
	let menuButton = $state<HTMLButtonElement>();

	onMount(() => {
		const mq = window.matchMedia(DRAWER_QUERY);
		const update = () => {
			drawer = mq.matches;
			if (!drawer) drawerOpen = false;
		};
		update();
		mq.addEventListener('change', update);
		return () => mq.removeEventListener('change', update);
	});

	afterNavigate(() => {
		drawerOpen = false;
	});

	function toggleCollapsed() {
		collapsed = !collapsed;
		storeSidebarCollapsed(collapsed);
	}

	async function closeDrawer() {
		drawerOpen = false;
		await tick();
		menuButton?.focus();
	}

	/** The top bar search box; hidden on the search page, which has its own. */
	const showQuickSearch = $derived(!/^\/search(?:\/|$)/.test(page.url.pathname));
</script>

<a class="skip-link" href="#main">Skip to content</a>

<div class="app-shell">
	<AppSidebar
		{groups}
		{collapsed}
		{drawer}
		open={drawerOpen}
		ontogglecollapsed={toggleCollapsed}
		onclose={closeDrawer}
	/>
	{#if drawer && drawerOpen}
		<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
		<div class="backdrop" aria-hidden="true" onclick={closeDrawer}></div>
	{/if}

	<div class="main-column">
		<header class="topbar">
			{#if drawer}
				<button
					class="btn btn-ghost btn-icon"
					type="button"
					aria-label="Open navigation"
					aria-controls="app-sidebar"
					aria-expanded={drawerOpen}
					bind:this={menuButton}
					onclick={() => (drawerOpen = true)}
				>
					<Icon name="menu" size={18} />
				</button>
				<a class="topbar-brand" href="/search"><Wordmark /></a>
			{/if}
			{#if showQuickSearch}
				<form class="quick-search" role="search" method="GET" action="/search">
					<Icon name="search" size={15} />
					<label class="visually-hidden" for="quick-search-input">Search code</label>
					<input
						id="quick-search-input"
						name="q"
						type="search"
						placeholder="Search code…"
						autocomplete="off"
						spellcheck="false"
					/>
				</form>
			{/if}
			<div class="spacer"></div>
			<div class="topbar-actions">
				{#if data.auth.user?.siteAdmin}
					<StatusMessagesNavItem />
				{/if}
				{#if data.auth.user}
					<UserMenu user={data.auth.user} {onsignout} />
				{:else}
					<a class="btn btn-sm" href="/sign-in">Sign in</a>
				{/if}
			</div>
		</header>

		{#if signOutError}
			<div class="alert alert-danger banner" role="alert">Sign out failed: {signOutError}</div>
		{/if}

		{#if data.auth.user}
			<CodeHostSignInBanner />
		{/if}

		<main id="main" tabindex="-1">
			{@render children()}
		</main>
	</div>
</div>

<style>
	.skip-link {
		position: absolute;
		left: -999px;
		top: var(--space-2);
		z-index: 100;
		padding: var(--space-2) var(--space-3);
		border-radius: var(--radius);
		background: var(--color-surface);
		box-shadow: var(--shadow-md);
	}
	.skip-link:focus {
		left: var(--space-2);
	}
	.app-shell {
		display: flex;
		align-items: flex-start;
		min-height: 100vh;
	}
	.main-column {
		flex: 1;
		min-width: 0;
		min-height: 100vh;
		display: flex;
		flex-direction: column;
	}
	.topbar {
		position: sticky;
		top: 0;
		z-index: 20;
		display: flex;
		align-items: center;
		gap: var(--space-2);
		height: var(--nav-height);
		padding: 0 var(--space-4);
		border-bottom: 1px solid var(--color-border);
		background: color-mix(in srgb, var(--color-bg) 88%, transparent);
		backdrop-filter: saturate(1.4) blur(8px);
	}
	.topbar-brand {
		color: var(--color-text);
		font-weight: var(--weight-semibold);
		letter-spacing: -0.02em;
		margin-right: var(--space-2);
	}
	.topbar-brand:hover {
		text-decoration: none;
	}
	.quick-search {
		position: relative;
		display: flex;
		align-items: center;
		flex: 0 1 26rem;
		min-width: 0;
		color: var(--color-text-subtle);
	}
	.quick-search :global(.icon) {
		position: absolute;
		left: 0.6rem;
		pointer-events: none;
	}
	.quick-search input {
		width: 100%;
		height: 1.875rem;
		padding: 0 0.6rem 0 2rem;
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
		background: var(--color-bg-subtle);
		color: var(--color-text);
		font-family: var(--font-mono);
		font-size: var(--text-sm);
		transition:
			border-color var(--duration) var(--ease),
			box-shadow var(--duration) var(--ease),
			background-color var(--duration) var(--ease);
	}
	.quick-search input::placeholder {
		font-family: var(--font-sans);
		color: var(--color-text-subtle);
	}
	.quick-search input:hover {
		border-color: var(--color-border-strong);
	}
	.quick-search input:focus {
		outline: none;
		background: var(--color-surface);
		border-color: var(--color-focus);
		box-shadow: var(--focus-ring);
	}
	.spacer {
		flex: 1;
	}
	.topbar-actions {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		flex: none;
	}
	.backdrop {
		position: fixed;
		inset: 0;
		z-index: 25;
		background: var(--overlay);
	}
	.banner {
		margin: var(--space-3) var(--space-4) 0;
	}
	main {
		flex: 1;
		min-width: 0;
		outline: none;
	}
	@media (max-width: 56.25rem) {
		.topbar {
			padding: 0 var(--space-2);
		}
	}
	@media (max-width: 30rem) {
		.topbar-brand {
			display: none;
		}
	}
</style>
