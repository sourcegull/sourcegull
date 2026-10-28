<!--
	Incoming code host webhook deliveries, newest first, for all connections or
	one (`?connection=<ExternalService ID>`): status, event, the selected headers and the body.
-->
<script lang="ts">
	import { page } from '$app/state';
	import { formatDate, relativeTime } from '#lib/batches/batch-change.ts';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		WEBHOOK_LOGS,
		prettyBody,
		statusClass,
		type WebhookLog,
		type WebhookLogsResult
	} from '#lib/admin/logs.ts';

	const PAGE = 50;

	const connection = $derived(page.url.searchParams.get('connection'));

	let items = $state.raw<WebhookLog[]>([]);
	let total = $state(0);
	let cursor = $state<string | null>(null);
	let hasMore = $state(false);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let onlyErrors = $state(false);
	let onlyUnmatched = $state(false);

	async function load(more = false): Promise<void> {
		loading = true;
		try {
			const r = (
				await graphql<WebhookLogsResult, Record<string, unknown>>(WEBHOOK_LOGS, {
					first: PAGE,
					after: more ? cursor : null,
					onlyErrors,
					onlyUnmatched,
					webhookID: connection
				})
			).webhookLogs;
			items = more ? [...items, ...r.nodes] : r.nodes;
			total = r.totalCount;
			cursor = r.pageInfo.endCursor;
			hasMore = r.pageInfo.hasNextPage;
			error = null;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loading = false;
		}
	}

	$effect(() => {
		void [onlyErrors, onlyUnmatched, connection];
		void load();
	});
</script>

<svelte:head><title>Webhook logs · Site admin · Sourcegull</title></svelte:head>

{#if connection}
	<p class="crumbs">
		<a href="/site-admin/external-services/{encodeURIComponent(connection)}">Code host connection</a> /
	</p>
{/if}

<div class="page-header">
	<div>
		<h1>Webhook logs</h1>
		<p class="page-description">
			Webhooks code hosts sent to Sourcegull{connection ? ' for this connection' : ''}, newest first, kept
			for a week. Secrets are never logged.
			{#if connection}<a href="/site-admin/webhook-logs">All connections</a>{/if}
		</p>
	</div>
	<div class="page-actions">
		<button class="btn" type="button" onclick={() => void load()} disabled={loading}>Refresh</button>
	</div>
</div>

<div class="toolbar list-toolbar">
	<label class="checkbox"><input type="checkbox" bind:checked={onlyErrors} /> Only errors</label>
	{#if !connection}
		<label class="checkbox"><input type="checkbox" bind:checked={onlyUnmatched} /> Only unmatched</label>
	{/if}
	<span class="muted small" aria-live="polite">{total.toLocaleString('en-US')} deliveries.</span>
</div>

{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}

<div class="table-scroll">
	<table class="table table-bordered table-sm" aria-busy={loading}>
		<thead>
			<tr>
				<th>Received</th>
				<th>Status</th>
				<th>Event</th>
				<th>Connection</th>
			</tr>
		</thead>
		<tbody>
			{#each items as w (w.id)}
				<tr data-testid="webhook-log">
					<td class="small nowrap" title={formatDate(w.receivedAt)}>{relativeTime(w.receivedAt)}</td>
					<td
						><span
							class="badge {statusClass(w.statusCode)}"
							class:badge-success={statusClass(w.statusCode) === 'ok'}
							class:badge-danger={statusClass(w.statusCode) === 'bad'}>{w.statusCode}</span
						></td
					>
					<td>
						<details>
							<summary>{w.event ?? 'unknown event'}</summary>
							<div class="muted small mono">{w.request.method} {w.request.url}</div>
							{#each w.request.headers as h (h.name)}<div class="mono small">{h.name}: {h.values.join(', ')}</div>{/each}
							<pre class="code">{prettyBody(w.request.body)}</pre>
						</details>
						{#if w.response.body}<pre class="code bad">{w.response.body}</pre>{/if}
					</td>
					<td>
						{#if w.externalService}
							<a href="/site-admin/external-services/{encodeURIComponent(w.externalService.id)}"
								>{w.externalService.displayName}</a
							>
						{:else}<span class="muted">unmatched</span>{/if}
					</td>
				</tr>
			{:else}
				<tr><td colspan="4" class="empty-cell">{loading ? 'Loading…' : 'No webhook deliveries logged.'}</td></tr>
			{/each}
		</tbody>
	</table>
</div>

{#if hasMore}
	<button class="btn more" type="button" onclick={() => void load(true)} disabled={loading}>Show older</button>
{/if}

<style>
	.crumbs {
		margin: 0 0 var(--space-2);
		font-size: var(--text-sm);
	}
	.small {
		font-size: var(--text-xs);
	}
	.list-toolbar {
		gap: var(--space-4);
		margin-bottom: var(--space-3);
	}
	.more {
		margin-top: var(--space-3);
	}
	.nowrap {
		white-space: nowrap;
	}
	pre.bad {
		color: var(--color-danger);
	}
	pre.code {
		margin: var(--space-1) 0;
		padding: var(--space-2);
		background: var(--color-bg-muted);
		border-radius: var(--radius);
		white-space: pre-wrap;
		overflow-wrap: anywhere;
		font-family: var(--font-mono);
		font-size: var(--text-xs);
		max-height: 20rem;
		overflow: auto;
	}
</style>
