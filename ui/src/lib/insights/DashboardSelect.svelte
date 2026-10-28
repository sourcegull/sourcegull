<script lang="ts">
	import { dashboardOwnerName, dashboardURL, groupDashboards } from './insights.ts';
	import type { DashboardNode, OrgRef } from './queries.ts';

	interface Props {
		current: DashboardNode | null;
		dashboards: DashboardNode[];
		orgs: OrgRef[];
	}

	let { current, dashboards, orgs }: Props = $props();

	let open = $state(false);
	let find = $state('');
	let root: HTMLDivElement;
	let findInput = $state<HTMLInputElement>();

	const groups = $derived(groupDashboards(dashboards, orgs, find));
	const title = $derived(dashboards.length === 0 ? 'No dashboards to select' : (current?.title ?? 'Unknown dashboard'));

	function toggle(): void {
		open = !open;
		if (open) {
			find = '';
			queueMicrotask(() => findInput?.focus());
		}
	}

	function onwindowclick(e: MouseEvent): void {
		if (open && !e.composedPath().includes(root)) open = false;
	}

	function onkeydown(e: KeyboardEvent): void {
		if (open && e.key === 'Escape') {
			open = false;
			root.querySelector<HTMLButtonElement>('button.trigger')?.focus();
		}
	}
</script>

<svelte:window onclick={onwindowclick} onkeydown={onkeydown} />

<div class="select" bind:this={root}>
	<button
		type="button"
		class="btn trigger"
		aria-haspopup="listbox"
		aria-expanded={open}
		aria-label={`Choose a dashboard, ${title}`}
		disabled={dashboards.length === 0}
		onclick={toggle}
	>
		<span class="title">{title}</span>
		{#if current}<span class="badge">{dashboardOwnerName(current, orgs)}</span>{/if}
		<span aria-hidden="true" class="caret">▾</span>
	</button>
	{#if open}
		<div class="popover card" role="dialog" aria-label="Choose a dashboard">
			<label class="visually-hidden" for="dashboard-find">Find a dashboard</label>
			<input
				id="dashboard-find"
				class="input"
				type="search"
				placeholder="Find a dashboard…"
				autocomplete="off"
				spellcheck="false"
				bind:this={findInput}
				bind:value={find}
			/>
			<div class="groups">
				{#each groups as g (g.key)}
					<div class="group" role="group" aria-label={g.heading}>
						<div class="heading">{g.heading}</div>
						{#each g.dashboards as d (d.id)}
							<a
								href={dashboardURL(d.id)}
								class="option"
								aria-current={d.id === current?.id ? 'true' : undefined}
								onclick={() => (open = false)}
							>
								<span class="name">{d.title}</span>
								<span class="badge">{dashboardOwnerName(d, orgs)}</span>
							</a>
						{/each}
					</div>
				{:else}
					<div class="empty">
						<span class="muted">No dashboards found.</span>
						<a href="/trends/add-dashboard" onclick={() => (open = false)}>Create a dashboard</a>
					</div>
				{/each}
			</div>
		</div>
	{/if}
</div>

<style>
	.select {
		position: relative;
		min-width: 0;
	}
	.trigger {
		max-width: 100%;
		min-width: min(18rem, 100%);
		justify-content: space-between;
	}
	.title {
		overflow: hidden;
		text-overflow: ellipsis;
		font-weight: 600;
	}
	.caret {
		margin-left: auto;
		color: var(--color-text-muted);
	}
	.popover {
		position: absolute;
		top: calc(100% + var(--space-1));
		left: 0;
		z-index: 20;
		width: min(24rem, calc(100vw - 2rem));
		padding: var(--space-2);
		box-shadow: var(--shadow-lg);
	}
	.groups {
		margin-top: var(--space-2);
		max-height: 20rem;
		overflow-y: auto;
	}
	.heading {
		padding: var(--space-2) var(--space-2) var(--space-1);
		font-size: 0.8rem;
		font-weight: 600;
		color: var(--color-text-muted);
		text-transform: uppercase;
		letter-spacing: 0.04em;
	}
	.option {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		padding: var(--space-1) var(--space-2);
		border-radius: var(--radius);
		color: var(--color-text);
	}
	.option:hover {
		background: var(--color-bg-muted);
		text-decoration: none;
	}
	.option[aria-current='true'] {
		background: var(--color-info-bg);
	}
	.name {
		flex: 1;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.empty {
		display: flex;
		flex-direction: column;
		gap: var(--space-1);
		padding: var(--space-2);
	}
</style>
