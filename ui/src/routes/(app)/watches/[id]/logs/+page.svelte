<script lang="ts">
	import { page } from '$app/state';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		MONITOR_EVENTS,
		actionLabel,
		type EventStatus,
		type MonitorEventsResult,
		type TriggerEventNode
	} from '#lib/monitors/queries.ts';
	import { searchQueryURL } from '#lib/search/query.ts';

	const PAGE_SIZE = 20;

	const id = $derived(page.params.id ?? '');

	let description = $state('');
	let query = $state('');
	let events = $state.raw<TriggerEventNode[]>([]);
	let totalCount = $state(0);
	let cursor = $state<string | null>(null);
	let hasMore = $state(false);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let open = $state<Record<string, boolean>>({});

	async function load(nodeId: string, after: string | null): Promise<void> {
		loading = true;
		error = null;
		try {
			const r = await graphql<MonitorEventsResult, { id: string; first: number; after: string | null }>(
				MONITOR_EVENTS,
				{ id: nodeId, first: PAGE_SIZE, after }
			);
			if (!r.node) {
				error = 'Watch not found.';
				return;
			}
			description = r.node.description;
			query = r.node.trigger.query;
			const c = r.node.trigger.events;
			events = after ? [...events, ...c.nodes] : c.nodes;
			totalCount = c.totalCount;
			cursor = c.pageInfo.endCursor;
			hasMore = c.pageInfo.hasNextPage;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loading = false;
		}
	}

	$effect(() => {
		void load(id, null);
	});

	function statusClass(s: EventStatus): string {
		return s === 'SUCCESS' ? 'ok badge-success' : s === 'ERROR' ? 'err badge-danger' : 'pending badge-warning';
	}

	const short = (oid: string | null) => (oid ? oid.slice(0, 7) : '');
</script>

<svelte:head><title>Logs · {description || 'Watch'} · Sourcegull</title></svelte:head>

<div class="page">
	<p class="crumbs">
		<a href="/watches">Watches</a> / <a href={`/watches/${id}`}>{description || '…'}</a> /
	</p>
	<div class="page-header">
		<div>
			<h1>Logs <span class="muted count">{totalCount} runs</span></h1>
			<p class="page-description">
				A run searches the commits that pushes added to the watched branches. Runs with results notify each enabled
				action; failed deliveries are retried with backoff.
			</p>
			{#if query}<p class="small muted query">Query: <a class="mono" href={searchQueryURL(query)}>{query}</a></p>{/if}
		</div>
		<div class="page-actions">
			<button type="button" class="btn" disabled={loading} onclick={() => load(id, null)}>Refresh</button>
		</div>
	</div>

	{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}

	<div class="table-scroll">
		<table class="table table-bordered">
			<thead>
				<tr><th>Status</th><th>Time</th><th>Results</th><th>Commits searched</th><th>Actions</th></tr>
			</thead>
			<tbody>
				{#each events as e (e.id)}
					<tr>
						<td><span class="badge status {statusClass(e.status)}">{e.status.toLowerCase()}</span></td>
						<td>{new Date(e.timestamp).toLocaleString()}</td>
						<td>{e.resultCount}</td>
						<td class="ranges">
							{#each e.ranges.slice(0, open[e.id] ? undefined : 3) as r, i (i)}
								<div>
									{r.repository}{r.revision ? `@${r.revision}` : ''}
									<span class="mono">{r.from ? `${short(r.from)}..${short(r.to)}` : `…${short(r.to)}`}</span>
								</div>
							{/each}
							{#if e.ranges.length > 3 && !open[e.id]}
								<button type="button" class="btn-link" onclick={() => (open[e.id] = true)}>
									{e.ranges.length - 3} more
								</button>
							{/if}
						</td>
						<td>
							{#each e.actions.nodes.filter((a) => a.events.nodes.length > 0) as a (a.id)}
								{#each a.events.nodes as ae (ae.id)}
									<div>
										{actionLabel(a.__typename)}:
										<span class="badge status {statusClass(ae.status)}">{ae.status.toLowerCase()}</span>
										{#if ae.message}<span class="muted message">{ae.message}</span>{/if}
									</div>
								{/each}
							{:else}
								<span class="muted">{e.resultCount > 0 || e.status !== 'SUCCESS' ? '—' : 'nothing to send'}</span>
							{/each}
						</td>
					</tr>
					{#if e.message}
						<tr class="message-row"><td colspan="5" class="muted message">{e.message}</td></tr>
					{/if}
				{:else}
					<tr><td colspan="5" class="muted">{loading ? 'Loading…' : 'No runs yet: pushes to watched repositories will show up here.'}</td></tr>
				{/each}
			</tbody>
		</table>
	</div>

	{#if hasMore}
		<button type="button" class="btn more" disabled={loading} onclick={() => load(id, cursor)}>
			{loading ? 'Loading…' : 'Show more'}
		</button>
	{/if}
</div>

<style>
	.count {
		font-size: var(--text-md);
		font-weight: var(--weight-regular);
		margin-left: var(--space-2);
	}
	.query {
		margin: var(--space-2) 0 0;
		overflow-wrap: anywhere;
	}
	.ranges {
		font-size: var(--text-sm);
		overflow-wrap: anywhere;
	}
	.message {
		font-size: var(--text-sm);
		overflow-wrap: anywhere;
	}
	.more {
		margin-top: var(--space-3);
	}
</style>
