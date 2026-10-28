<!--
	Outbound webhooks: URLs Sourcegull posts signed events to, with their event types and
	delivery counts, and the delivery log of watch actions that post to a URL (only the host
	is shown, as the URL may hold a token). When the resources file owns every webhook, none
	can be added here.
-->
<script lang="ts">
	import { onMount } from 'svelte';
	import { formatDate, relativeTime } from '#lib/batches/batch-change.ts';
	import ManagedBadge from '#lib/components/ManagedBadge.svelte';
	import ManagedNotice from '#lib/components/ManagedNotice.svelte';
	import { isExclusive } from '#lib/declared-config.ts';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import { statusClass } from '#lib/admin/logs.ts';
	import {
		CHANNEL_DELIVERY_LOGS,
		OUTBOUND_WEBHOOKS,
		channelName,
		describeEventTypes,
		type ChannelDeliveryLog,
		type ChannelDeliveryLogsResult,
		type OutboundWebhookSummary,
		type OutboundWebhooksResult
	} from '#lib/admin/outbound-webhooks.ts';

	const PAGE = 50;

	let hooks = $state.raw<OutboundWebhookSummary[]>([]);
	let total = $state(0);
	let cursor = $state<string | null>(null);
	let hasMore = $state(false);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let exclusive = $state(false);

	async function load(more = false): Promise<void> {
		loading = true;
		try {
			const data = await graphql<OutboundWebhooksResult, { first: number; after: string | null }>(
				OUTBOUND_WEBHOOKS,
				{ first: PAGE, after: more ? cursor : null }
			);
			const r = data.outboundWebhooks;
			exclusive = isExclusive(data.site.declaredConfigFiles, 'outboundWebhooks');
			hooks = more ? [...hooks, ...r.nodes] : r.nodes;
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

	onMount(() => void load());

	const DELIVERY_PAGE = 20;

	let deliveries = $state.raw<ChannelDeliveryLog[]>([]);
	let deliveryTotal = $state(0);
	let deliveryCursor = $state<string | null>(null);
	let deliveriesMore = $state(false);
	let deliveriesLoading = $state(true);
	let deliveryError = $state<string | null>(null);
	let onlyErrors = $state(false);

	async function loadDeliveries(more = false): Promise<void> {
		deliveriesLoading = true;
		try {
			const data = await graphql<ChannelDeliveryLogsResult, Record<string, unknown>>(CHANNEL_DELIVERY_LOGS, {
				first: DELIVERY_PAGE,
				after: more ? deliveryCursor : null,
				onlyErrors
			});
			const r = data.channelDeliveryLogs;
			deliveries = more ? [...deliveries, ...r.nodes] : r.nodes;
			deliveryTotal = r.totalCount;
			deliveryCursor = r.pageInfo.endCursor;
			deliveriesMore = r.pageInfo.hasNextPage;
			deliveryError = null;
		} catch (err) {
			deliveryError = errorMessage(err);
		} finally {
			deliveriesLoading = false;
		}
	}

	$effect(() => {
		void onlyErrors;
		void loadDeliveries();
	});
</script>

<svelte:head><title>Outbound webhooks · Site admin · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Outbound webhooks</h1>
		<p class="page-description">
			Sourcegull posts bulk change and changeset events to these URLs, signed with each webhook's
			secret, and retries failed deliveries with backoff. Delivery logs are kept for
			<code class="code-inline">webhooks.outbound.logRetention</code>.
		</p>
	</div>
	{#if !exclusive}
		<div class="page-actions">
			<a class="btn btn-primary" href="/site-admin/outbound-webhooks/new">New webhook</a>
		</div>
	{/if}
</div>

{#if exclusive}<ManagedNotice kind="outboundWebhooks" />{/if}

{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}

<div class="table-scroll">
	<table class="table table-bordered" aria-busy={loading}>
		<thead>
			<tr>
				<th>URL</th>
				<th>Event types</th>
				<th class="num">Deliveries</th>
				<th>Updated</th>
			</tr>
		</thead>
		<tbody>
			{#each hooks as h (h.id)}
				<tr data-testid="outbound-webhook">
					<td>
						<a class="mono" href="/site-admin/outbound-webhooks/{encodeURIComponent(h.id)}">{h.url}</a>
						<ManagedBadge managedBy={h.managedBy} />
					</td>
					<td class="small">{describeEventTypes(h.eventTypes)}</td>
					<td class="num">
						{h.stats.total.toLocaleString('en-US')}
						{#if h.stats.errored > 0}<span class="bad">({h.stats.errored.toLocaleString('en-US')} failed)</span>{/if}
					</td>
					<td class="small">{relativeTime(h.updatedAt)}</td>
				</tr>
			{:else}
				<tr><td colspan="4" class="empty-cell">{loading ? 'Loading…' : 'No outbound webhooks yet.'}</td></tr>
			{/each}
		</tbody>
	</table>
</div>
<p class="muted small count">{total.toLocaleString('en-US')} webhooks.</p>

{#if hasMore}
	<button class="btn" type="button" onclick={() => void load(true)} disabled={loading}>Show more</button>
{/if}

<section class="section">
	<div class="section-header">
		<h2>Watch deliveries</h2>
		<label class="checkbox"><input type="checkbox" bind:checked={onlyErrors} /> Only failed</label>
	</div>
	<p class="muted small">
		Attempts of watch actions that post to a webhook, Slack, Mattermost or Telegram. Only the host of each
		destination is shown.
	</p>
	{#if deliveryError}<div class="alert alert-danger" role="alert">{deliveryError}</div>{/if}
	<div class="table-scroll">
		<table class="table table-bordered table-sm" aria-busy={deliveriesLoading}>
			<thead>
				<tr>
					<th>Sent</th>
					<th>Watch</th>
					<th>Channel</th>
					<th>Host</th>
					<th>Status</th>
				</tr>
			</thead>
			<tbody>
				{#each deliveries as d (d.id)}
					<tr data-testid="channel-delivery">
						<td class="small nowrap" title={formatDate(d.sentAt)}>{relativeTime(d.sentAt)}</td>
						<td>
							{#if d.watchId}
								<a href="/watches/{encodeURIComponent(d.watchId)}">{d.watchDescription}</a>
							{:else}
								<span class="muted">Deleted watch</span>
							{/if}
						</td>
						<td>{channelName(d.channel)}</td>
						<td class="mono small">{d.host ?? ''}</td>
						<td>
							<span
								class="badge {statusClass(d.statusCode)}"
								class:badge-success={statusClass(d.statusCode) === 'ok'}
								class:badge-danger={statusClass(d.statusCode) === 'bad'}>{d.statusCode || 'no response'}</span
							>
							{#if d.error}<div class="small bad error">{d.error}</div>{/if}
						</td>
					</tr>
				{:else}
					<tr>
						<td colspan="5" class="empty-cell">{deliveriesLoading ? 'Loading…' : 'No watch deliveries logged.'}</td>
					</tr>
				{/each}
			</tbody>
		</table>
	</div>
	<p class="muted small count">{deliveryTotal.toLocaleString('en-US')} attempts.</p>
	{#if deliveriesMore}
		<button class="btn" type="button" onclick={() => void loadDeliveries(true)} disabled={deliveriesLoading}
			>Show older</button
		>
	{/if}
</section>

<style>
	.small {
		font-size: var(--text-xs);
	}
	.count {
		margin: var(--space-2) 0 0;
	}
	.bad {
		color: var(--color-danger);
	}
	td .mono {
		overflow-wrap: anywhere;
	}
	.nowrap {
		white-space: nowrap;
	}
	.error {
		overflow-wrap: anywhere;
		margin-top: var(--space-1);
	}
</style>
