<!--
	The outbound request log: code host API calls with secrets redacted, newest
	first. Off unless the site configuration sets `outboundRequestLogLimit`.
-->
<script lang="ts">
	import { onMount } from 'svelte';
	import { formatDate, relativeTime } from '#lib/batches/batch-change.ts';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		OUTBOUND_REQUESTS,
		formatMs,
		prettyBody,
		statusClass,
		type OutboundRequest,
		type OutboundRequestsResult
	} from '#lib/admin/logs.ts';

	const PAGE = 50;

	let items = $state.raw<OutboundRequest[]>([]);
	let total = $state(0);
	let cursor = $state<string | null>(null);
	let hasMore = $state(false);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let filter = $state('');

	async function load(more = false): Promise<void> {
		loading = true;
		try {
			const r = (
				await graphql<OutboundRequestsResult, { first: number; after: string | null }>(OUTBOUND_REQUESTS, {
					first: PAGE,
					after: more ? cursor : null
				})
			).outboundRequests;
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

	const shown = $derived(
		filter.trim()
			? items.filter((i) => `${i.method} ${i.url} ${i.statusCode}`.toLowerCase().includes(filter.trim().toLowerCase()))
			: items
	);

	onMount(() => void load());
</script>

<svelte:head><title>Outbound requests · Site admin · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Outbound requests</h1>
		<p class="page-description">
			Requests Sourcegull sent to code hosts, newest first, with tokens and other secrets redacted. The
			log keeps the newest <code class="code-inline">outboundRequestLogLimit</code> requests (site
			configuration); with the default of 0 nothing is logged.
		</p>
	</div>
	<div class="page-actions">
		<button class="btn" type="button" onclick={() => void load()} disabled={loading}>Refresh</button>
	</div>
</div>

<div class="toolbar list-toolbar">
	<input class="input filter" type="search" placeholder="Filter by method, URL or status" bind:value={filter} aria-label="Filter" />
	<span class="muted small" aria-live="polite">{total.toLocaleString('en-US')} requests logged.</span>
</div>

{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}

<div class="table-scroll">
	<table class="table table-bordered table-sm" aria-busy={loading}>
		<thead>
			<tr>
				<th>Sent</th>
				<th>Request</th>
				<th>Status</th>
				<th class="num">Duration</th>
			</tr>
		</thead>
		<tbody>
			{#each shown as r (r.id)}
				<tr data-testid="outbound-request">
					<td class="small nowrap" title={formatDate(r.startedAt)}>{relativeTime(r.startedAt)}</td>
					<td>
						<details>
							<summary><span class="mono">{r.method} {r.url}</span></summary>
							<dl class="facts small">
								<dt>Client</dt>
								<dd>{r.creationStackFrame || '—'}</dd>
								{#if r.callStack}<dt>Caller</dt><dd class="mono">{r.callStack}</dd>{/if}
								<dt>Request headers</dt>
								<dd>
									{#each r.requestHeaders as h (h.name)}<div class="mono">{h.name}: {h.values.join(', ')}</div>{/each}
								</dd>
								{#if r.requestBody}<dt>Request body</dt><dd><pre class="code">{prettyBody(r.requestBody)}</pre></dd>{/if}
								<dt>Response headers</dt>
								<dd>
									{#each r.responseHeaders as h (h.name)}<div class="mono">{h.name}: {h.values.join(', ')}</div>{:else}—{/each}
								</dd>
							</dl>
						</details>
						{#if r.errorMessage}<pre class="code bad">{r.errorMessage}</pre>{/if}
					</td>
					<td
						><span
							class="badge {statusClass(r.statusCode)}"
							class:badge-success={statusClass(r.statusCode) === 'ok'}
							class:badge-danger={statusClass(r.statusCode) === 'bad'}
							>{r.statusCode || 'no response'}</span
						></td
					>
					<td class="num">{formatMs(r.durationMs)}</td>
				</tr>
			{:else}
				<tr><td colspan="4" class="empty-cell">{loading ? 'Loading…' : 'No outbound requests logged.'}</td></tr>
			{/each}
		</tbody>
	</table>
</div>

{#if hasMore}
	<button class="btn more" type="button" onclick={() => void load(true)} disabled={loading}>Show older</button>
{/if}

<style>
	.small {
		font-size: var(--text-xs);
	}
	.list-toolbar {
		margin-bottom: var(--space-3);
	}
	.filter {
		max-width: 24rem;
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
	.facts dt {
		color: var(--color-text-muted);
	}
	.facts {
		display: grid;
		grid-template-columns: max-content 1fr;
		gap: var(--space-1) var(--space-3);
	}
	.facts dd {
		margin: 0;
		overflow-wrap: anywhere;
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
	summary .mono {
		overflow-wrap: anywhere;
	}
</style>
