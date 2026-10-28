<script lang="ts">
	import { logEvent } from '#lib/events.ts';
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import MonitorForm, { type MonitorFields } from '#lib/monitors/MonitorForm.svelte';
	import {
		DELETE_MONITOR,
		MONITOR,
		RESET_TRIGGER,
		UPDATE_MONITOR,
		actionDraft,
		actionEditInput,
		canShareUnfiltered,
		viewerNamespaces,
		type MonitorNode,
		type MonitorResult
	} from '#lib/monitors/queries.ts';
	import { namespaceLabel } from '#lib/search/queries.ts';

	const id = $derived(page.params.id ?? '');
	const user = $derived(page.data.auth?.user ?? null);

	let monitor = $state.raw<MonitorNode | null>(null);
	let namespaces = $state.raw<{ id: string; label: string }[]>([]);
	let canUnfilter = $state(false);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let saveError = $state<string | null>(null);
	let notice = $state<string | null>(null);
	let busy = $state(false);
	// Bumped after a save so the form is seeded again from the stored watch.
	let version = $state(0);

	async function load(nodeId: string): Promise<void> {
		loading = true;
		error = null;
		try {
			const [r, ns, can] = await Promise.all([
				graphql<MonitorResult, { id: string }>(MONITOR, { id: nodeId }),
				namespaces.length > 0 ? Promise.resolve(namespaces) : viewerNamespaces(),
				canShareUnfiltered()
			]);
			namespaces = ns;
			canUnfilter = can;
			monitor = r.node && r.node.__typename === 'Monitor' ? r.node : null;
			if (!monitor) error = 'Watch not found.';
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loading = false;
		}
	}

	$effect(() => {
		void load(id);
	});

	/** The owner and recipients must be selectable even when they are not the viewer's. */
	const choices = $derived.by(() => {
		const out = [...namespaces];
		const add = (n: { id: string; __typename: 'User' | 'Org'; username?: string; name?: string }) => {
			if (!out.some((o) => o.id === n.id)) out.push({ id: n.id, label: namespaceLabel(n) });
		};
		if (monitor) {
			add(monitor.owner);
			for (const a of monitor.actions.nodes) {
				if (a.__typename === 'MonitorEmail') a.recipients.nodes.forEach(add);
			}
		}
		return out;
	});

	async function save(v: MonitorFields): Promise<void> {
		if (!monitor) return;
		busy = true;
		saveError = null;
		notice = null;
		try {
			await graphql<unknown, Record<string, unknown>>(UPDATE_MONITOR, {
				monitor: {
					id: monitor.id,
					update: {
						namespace: v.namespace,
						description: v.description,
						enabled: v.enabled,
						unfilteredResults: v.unfiltered
					}
				},
				trigger: { id: monitor.trigger.id, update: { query: v.query } },
				actions: v.actions.map(actionEditInput)
			});
			await load(id);
			version += 1;
			notice = 'Saved.';
		} catch (err) {
			saveError = errorMessage(err);
		} finally {
			busy = false;
		}
	}

	async function remove(): Promise<void> {
		if (!monitor || !confirm(`Delete the watch “${monitor.description}” and its logs?`)) return;
		busy = true;
		try {
			await graphql<unknown, { id: string }>(DELETE_MONITOR, { id: monitor.id });
			await goto('/watches');
		} catch (err) {
			saveError = errorMessage(err);
		} finally {
			busy = false;
		}
	}

	async function reset(): Promise<void> {
		if (!monitor) return;
		busy = true;
		notice = null;
		try {
			await graphql<unknown, { id: string }>(RESET_TRIGGER, { id: monitor.trigger.id });
			notice = 'The next run searches the whole history of the watched branches.';
		} catch (err) {
			saveError = errorMessage(err);
		} finally {
			busy = false;
		}
	}

	// A usage event for the admin analytics.
	$effect(() => {
		if (page.params.id) logEvent('CodeMonitorPageViewed');
	});
</script>

<svelte:head><title>{monitor?.description ?? 'Watch'} · Sourcegull</title></svelte:head>

<div class="page page-narrow">
	<p class="crumbs"><a href="/watches">Watches</a> /</p>
	{#if loading && !monitor}
		<p class="loading"><span class="spinner"></span>Loading…</p>
	{:else if error}
		<div class="alert alert-danger" role="alert">{error}</div>
	{:else if monitor}
		<div class="page-header">
			<div>
				<h1 class="title">
					{monitor.description}
					<span class="badge" class:badge-success={monitor.enabled}>{monitor.enabled ? 'enabled' : 'disabled'}</span>
					{#if monitor.unfilteredResults}<span class="badge badge-danger">unfiltered</span>{/if}
				</h1>
				<p class="page-description">
					Owned by {namespaceLabel(monitor.owner)} · created {new Date(monitor.createdAt).toLocaleString()}
					{#if monitor.runsAs}· runs as @{monitor.runsAs.username}{/if}
				</p>
			</div>
			<div class="page-actions">
				<a class="btn" href={`${monitor.url}/logs`}>Logs</a>
			</div>
		</div>
		{#if notice}<div class="alert alert-success" role="status">{notice}</div>{/if}
		{#if monitor.unfilteredResults}
			<div class="alert alert-danger" role="alert">
				<strong>Unfiltered:</strong> every email recipient gets all results found with @{monitor.runsAs?.username}'s access,
				including results in repositories they cannot read. Turned on by @{monitor.unfilteredBy?.username}{#if monitor.unfilteredAt}
					on {new Date(monitor.unfilteredAt).toLocaleString()}{/if}.
			</div>
		{/if}
		{#key version}
			<MonitorForm
				initial={{
					namespace: monitor.owner.id,
					description: monitor.description,
					query: monitor.trigger.query,
					enabled: monitor.enabled,
					actions: monitor.actions.nodes.map(actionDraft),
					unfiltered: monitor.unfilteredResults && monitor.unfilteredBy?.username === user?.username
				}}
				namespaces={choices}
				submitLabel="Save changes"
				viewer={user?.username ?? ''}
				siteAdmin={user?.siteAdmin ?? false}
				canShareUnfiltered={canUnfilter}
				stored={{
					runsAs: monitor.runsAs?.username ?? null,
					unfilteredBy: monitor.unfilteredResults ? (monitor.unfilteredBy?.username ?? null) : null,
					unfilteredAt: monitor.unfilteredAt
				}}
				error={saveError}
				{busy}
				onsubmit={save}
			/>
		{/key}
		<div class="toolbar danger-zone">
			<span class="muted small zone-label">Danger zone</span>
			<button type="button" class="btn btn-danger" disabled={busy} onclick={remove}>Delete watch</button>
			{#if user?.siteAdmin}
				<button type="button" class="btn" disabled={busy} onclick={reset}>Search the whole history once</button>
			{/if}
		</div>
	{/if}
</div>

<style>
	.title {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: var(--space-2);
	}
	.danger-zone {
		margin-top: var(--space-6);
		padding: var(--space-4);
		border: 1px solid var(--color-danger-border);
		border-radius: var(--radius-lg);
	}
	.zone-label {
		flex-basis: 100%;
		font-weight: var(--weight-medium);
		color: var(--color-danger);
	}
</style>
