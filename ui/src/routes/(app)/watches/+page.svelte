<script lang="ts">
	import type { ToggleCodeMonitorMutation } from '#lib/graphql-types.ts';
	import Icon from '#lib/components/Icon.svelte';
	import { logEvent } from '#lib/events.ts';
	import { page } from '$app/state';
	import { onMount } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		TOGGLE_MONITOR,
		USER_MONITORS,
		actionLabel,
		type MonitorNode,
		type UserMonitorsResult
	} from '#lib/monitors/queries.ts';
	import { namespaceLabel } from '#lib/search/queries.ts';
	import { searchQueryURL } from '#lib/search/query.ts';

	const PAGE_SIZE = 50;

	const user = $derived(page.data.auth?.user ?? null);

	let items = $state.raw<MonitorNode[]>([]);
	let totalCount = $state(0);
	let cursor = $state<string | null>(null);
	let hasMore = $state(false);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let toggling = $state<string | null>(null);

	async function load(after: string | null): Promise<void> {
		if (!user) return;
		loading = true;
		error = null;
		try {
			const r = await graphql<UserMonitorsResult, { user: string; first: number; after: string | null }>(
				USER_MONITORS,
				{ user: user.id, first: PAGE_SIZE, after }
			);
			const c = r.node?.monitors;
			if (!c) return;
			items = after ? [...items, ...c.nodes] : c.nodes;
			totalCount = c.totalCount;
			cursor = c.pageInfo.endCursor;
			hasMore = c.pageInfo.hasNextPage;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loading = false;
		}
	}

	onMount(() => {
		void load(null);
	});

	async function toggle(m: MonitorNode): Promise<void> {
		toggling = m.id;
		error = null;
		try {
			const r = await graphql<
				ToggleCodeMonitorMutation,
				{ id: string; enabled: boolean }
			>(TOGGLE_MONITOR, {
				id: m.id,
				enabled: !m.enabled
			});
			items = items.map((x) => (x.id === m.id ? { ...x, enabled: r.toggleCodeMonitor.enabled } : x));
		} catch (err) {
			error = errorMessage(err);
		} finally {
			toggling = null;
		}
	}

	// A usage event for the admin analytics.
	$effect(() => {
		logEvent('CodeMonitoringPageViewed');
	});
</script>

<svelte:head><title>Watches · Sourcegull</title></svelte:head>

<div class="page">
	<div class="page-header">
		<div>
			<h1>Watches</h1>
			<p class="page-description">
				Watch a commit or diff search and get notified by email, webhook, Slack, Mattermost or Telegram when new pushes match it.
			</p>
		</div>
		<div class="page-actions">
			<a class="btn btn-primary" href="/watches/new"><Icon name="plus" size={16} />New watch</a>
		</div>
	</div>

	{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}

	{#if items.length === 0 && !loading && !error}
		<div class="empty-state">
			<span class="icon"><Icon name="monitor" size={32} /></span>
			<h3>No watches yet</h3>
			<p>Get notified when someone pushes code that matches a search, such as a new use of a deprecated API.</p>
			<a class="btn btn-primary" href="/watches/new"><Icon name="plus" size={16} />Create a watch</a>
		</div>
	{:else}
		<div class="list-head">
			<span class="muted small">{totalCount} {totalCount === 1 ? 'watch' : 'watches'}</span>
		</div>
		<ul class="list list-group" aria-busy={loading}>
			{#each items as m (m.id)}
				<li>
					<div class="list-item-title">
						<a href={m.url}>{m.description}</a>
						<span class="badge" class:badge-success={m.enabled}>{m.enabled ? 'enabled' : 'disabled'}</span>
						<span class="push">
							<a class="btn btn-sm btn-ghost" href={`${m.url}/logs`}>Logs</a>
							<button type="button" class="btn btn-sm" disabled={toggling === m.id} onclick={() => toggle(m)}>
								{m.enabled ? 'Disable' : 'Enable'}
							</button>
						</span>
					</div>
					<a class="query mono" href={searchQueryURL(m.trigger.query)}>{m.trigger.query}</a>
					<p class="list-item-meta">
						{namespaceLabel(m.owner)}
						· {m.actions.nodes.length === 0
							? 'no actions'
							: m.actions.nodes.map((a) => actionLabel(a.__typename)).join(', ')}
					</p>
				</li>
			{:else}
				<li class="empty">Loading…</li>
			{/each}
		</ul>
	{/if}

	{#if hasMore}
		<div class="more">
			<button type="button" class="btn" disabled={loading} onclick={() => load(cursor)}>
				{loading ? 'Loading…' : 'Show more'}
			</button>
		</div>
	{/if}
</div>

<style>
	.list-head {
		display: flex;
		justify-content: flex-end;
		margin-bottom: var(--space-2);
	}
	.query {
		font-size: var(--text-sm);
		overflow-wrap: anywhere;
		color: var(--color-text-muted);
	}
	.query:hover {
		color: var(--color-link);
	}
	.more {
		margin-top: var(--space-4);
		text-align: center;
	}
</style>
