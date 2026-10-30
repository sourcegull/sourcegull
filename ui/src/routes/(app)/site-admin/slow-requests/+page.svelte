<!--
	The slow request log: GraphQL requests and searches slower than
	`observability.logSlowGraphQLRequests` / `observability.logSlowSearches`, newest first.
-->
<script lang="ts">
	import RelatedLinks from '#lib/components/RelatedLinks.svelte';
	import { formatDate, relativeTime } from '#lib/batches/batch-change.ts';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		SLOW_REQUESTS,
		formatMs,
		prettyBody,
		type SlowRequest,
		type SlowRequestKind,
		type SlowRequestsResult
	} from '#lib/admin/logs.ts';

	const PAGE = 50;

	let items = $state.raw<SlowRequest[]>([]);
	let total = $state(0);
	let cursor = $state<string | null>(null);
	let hasMore = $state(false);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let kind = $state<SlowRequestKind | ''>('');

	async function load(more = false): Promise<void> {
		loading = true;
		try {
			const r = (
				await graphql<SlowRequestsResult, { first: number; after: string | null; kind: SlowRequestKind | null }>(
					SLOW_REQUESTS,
					{ first: PAGE, after: more ? cursor : null, kind: kind || null }
				)
			).slowRequests;
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
		void kind;
		void load();
	});
</script>

<svelte:head><title>Slow requests · Site admin · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Slow requests</h1>
		<p class="page-description">
			GraphQL requests slower than <code class="code-inline">observability.logSlowGraphQLRequests</code>
			and searches slower than <code class="code-inline">observability.logSlowSearches</code>
			(milliseconds, site configuration; unset turns the log off). The newest 1,024 are kept; each is
			also written to the server log.
		</p>
		<RelatedLinks />
	</div>
	<div class="page-actions">
		<button class="btn" type="button" onclick={() => void load()} disabled={loading}>Refresh</button>
	</div>
</div>

<div class="toolbar list-toolbar">
	<select class="input kind" bind:value={kind} aria-label="Kind">
		<option value="">GraphQL and searches</option>
		<option value="GRAPHQL">GraphQL requests</option>
		<option value="SEARCH">Searches</option>
	</select>
	<span class="muted small" aria-live="polite">{total.toLocaleString('en-US')} slow requests logged.</span>
</div>

{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}

<div class="table-scroll">
	<table class="table table-bordered table-sm" aria-busy={loading}>
		<thead>
			<tr>
				<th>Started</th>
				<th>Request</th>
				<th>User</th>
				<th class="num">Duration</th>
			</tr>
		</thead>
		<tbody>
			{#each items as r (r.index)}
				<tr data-testid="slow-request">
					<td class="small nowrap" title={formatDate(r.start)}>{relativeTime(r.start)}</td>
					<td>
						<details>
							<summary>
								<span class="badge">{r.kind === 'SEARCH' ? 'search' : 'GraphQL'}</span>
								<span class="mono">{r.kind === 'SEARCH' ? r.query : r.name}</span>
								<span class="muted small">· {r.source}</span>
							</summary>
							<pre class="code">{r.query}</pre>
							{#if r.variables && r.variables !== '{}'}
								<div class="muted small">Variables</div>
								<pre class="code">{prettyBody(r.variables)}</pre>
							{/if}
						</details>
						{#each r.errors as e, i (i)}<pre class="code bad">{e}</pre>{/each}
					</td>
					<td>{r.user?.username ?? 'anonymous'}</td>
					<td class="num">{formatMs(r.duration * 1000)}</td>
				</tr>
			{:else}
				<tr><td colspan="4" class="empty-cell">{loading ? 'Loading…' : 'No slow requests logged.'}</td></tr>
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
	.kind {
		width: auto;
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
	summary .mono {
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
</style>
