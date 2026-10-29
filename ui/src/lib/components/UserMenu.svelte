<script lang="ts">
	import type { AuthUser } from '#lib/auth.ts';
	import { loadTheme, setTheme, type ThemePreference } from '#lib/theme/theme.ts';
	import Icon, { type IconName } from './Icon.svelte';

	interface Props {
		user: AuthUser;
		onsignout: () => void;
	}

	let { user, onsignout }: Props = $props();

	let open = $state(false);
	let theme = $state<ThemePreference>(loadTheme());
	let root: HTMLDivElement;

	const themes: { value: ThemePreference; label: string; icon: IconName }[] = [
		{ value: 'system', label: 'System', icon: 'system' },
		{ value: 'light', label: 'Light', icon: 'sun' },
		{ value: 'dark', label: 'Dark', icon: 'moon' }
	];

	function chooseTheme(value: ThemePreference) {
		theme = value;
		setTheme(value);
	}

	function onWindowClick(event: MouseEvent) {
		if (open && event.target instanceof Node && !root.contains(event.target)) open = false;
	}

	function onKeydown(event: KeyboardEvent) {
		if (open && event.key === 'Escape') {
			open = false;
			root.querySelector<HTMLButtonElement>('button.trigger')?.focus();
		}
	}
</script>

<svelte:window onclick={onWindowClick} onkeydown={onKeydown} />

<div class="user-menu" bind:this={root}>
	<button
		class="trigger"
		type="button"
		aria-haspopup="menu"
		aria-expanded={open}
		onclick={() => (open = !open)}
	>
		<span class="avatar" aria-hidden="true">{user.username.slice(0, 1).toUpperCase()}</span>
		<span class="username">{user.username}</span>
		<Icon name="chevron-down" size={14} />
	</button>
	{#if open}
		<div class="menu popover-surface" role="menu">
			<div class="menu-header">
				<span class="avatar avatar-lg" aria-hidden="true">{user.username.slice(0, 1).toUpperCase()}</span>
				<div class="who">
					<span class="muted">Signed in as</span>
					<strong>{user.username}</strong>
					{#if user.siteAdmin}<span class="badge badge-primary">site admin</span>{/if}
				</div>
			</div>
			<div class="separator" role="separator"></div>
			<a role="menuitem" href="/user/settings" onclick={() => (open = false)}>
				<Icon name="settings" />Settings
			</a>
			<a role="menuitem" href="/user/settings/tokens" onclick={() => (open = false)}>
				<Icon name="key" />Access tokens
			</a>
			<a role="menuitem" href="/user/settings/organizations" onclick={() => (open = false)}>
				<Icon name="org" />Organizations
			</a>
			<a role="menuitem" href="/teams" onclick={() => (open = false)}><Icon name="users" />Teams</a>
			{#if user.siteAdmin}
				<a role="menuitem" href="/site-admin" onclick={() => (open = false)}><Icon name="admin" />Site admin</a>
			{/if}
			<div class="separator" role="separator"></div>
			<div class="theme" role="group" aria-label="Theme">
				<span>Theme</span>
				<div class="segmented">
					{#each themes as t (t.value)}
						<button
							type="button"
							role="menuitemradio"
							aria-checked={theme === t.value}
							aria-label="{t.label} theme"
							title={t.label}
							onclick={() => chooseTheme(t.value)}
						>
							<Icon name={t.icon} size={14} />
						</button>
					{/each}
				</div>
			</div>
			<div class="separator" role="separator"></div>
			<button
				role="menuitem"
				type="button"
				onclick={() => {
					open = false;
					onsignout();
				}}><Icon name="signout" />Sign out</button
			>
		</div>
	{/if}
</div>

<style>
	.user-menu {
		position: relative;
	}
	.trigger {
		display: inline-flex;
		align-items: center;
		gap: var(--space-2);
		height: var(--control-height);
		padding: 0 var(--space-2) 0 var(--space-1);
		border: 1px solid transparent;
		border-radius: var(--radius-full);
		background: none;
		color: var(--color-text);
		font: inherit;
		font-weight: var(--weight-medium);
		cursor: pointer;
		transition: background-color var(--duration) var(--ease);
	}
	.trigger:hover,
	.trigger[aria-expanded='true'] {
		background: var(--color-bg-hover);
	}
	.trigger:focus-visible {
		outline: none;
		box-shadow: var(--focus-ring);
	}
	.trigger :global(.icon) {
		color: var(--color-text-muted);
	}
	@media (max-width: 40rem) {
		.username {
			display: none;
		}
	}
	.avatar {
		display: inline-grid;
		place-items: center;
		flex: none;
		width: 1.5rem;
		height: 1.5rem;
		border-radius: 50%;
		background: linear-gradient(135deg, var(--accent-400), var(--accent-700));
		color: #fff;
		font-size: var(--text-xs);
		font-weight: var(--weight-semibold);
	}
	.avatar-lg {
		width: 2.25rem;
		height: 2.25rem;
		font-size: var(--text-md);
	}
	.menu {
		position: absolute;
		right: 0;
		top: calc(100% + 6px);
		min-width: 15rem;
		padding: var(--space-1);
		z-index: 40;
	}
	.menu-header {
		display: flex;
		align-items: center;
		gap: var(--space-3);
		padding: var(--space-2);
	}
	.who {
		display: flex;
		flex-direction: column;
		align-items: flex-start;
		gap: 2px;
		min-width: 0;
		overflow-wrap: anywhere;
		font-size: var(--text-sm);
	}
	.menu a,
	.menu > button {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		width: 100%;
		text-align: left;
		padding: 0.375rem var(--space-2);
		border: none;
		border-radius: var(--radius);
		color: var(--color-text);
		background: none;
		font: inherit;
		cursor: pointer;
	}
	.menu a :global(.icon),
	.menu > button :global(.icon) {
		color: var(--color-text-muted);
	}
	.menu a:hover,
	.menu > button:hover,
	.menu a:focus-visible,
	.menu > button:focus-visible {
		background: var(--color-bg-hover);
		text-decoration: none;
		outline: none;
	}
	.separator {
		height: 1px;
		margin: var(--space-1) calc(-1 * var(--space-1));
		background: var(--color-border);
	}
	.theme {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--space-2);
		padding: var(--space-1) var(--space-2);
	}
	.segmented {
		display: inline-flex;
		padding: 2px;
		border-radius: var(--radius);
		background: var(--color-bg-muted);
		border: 1px solid var(--color-border);
	}
	.segmented button {
		display: inline-grid;
		place-items: center;
		width: 1.75rem;
		height: 1.5rem;
		border: none;
		border-radius: var(--radius-sm);
		background: none;
		color: var(--color-text-muted);
		cursor: pointer;
	}
	.segmented button:hover {
		color: var(--color-text);
	}
	.segmented button[aria-checked='true'] {
		background: var(--color-surface);
		color: var(--color-text);
		box-shadow: var(--shadow-xs);
	}
	.segmented button:focus-visible {
		outline: none;
		box-shadow: var(--focus-ring);
	}
</style>
