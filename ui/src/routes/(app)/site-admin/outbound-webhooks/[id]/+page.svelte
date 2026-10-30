<!--
	An outbound webhook: edit its URL, secret and event types, delete it, and read its
	delivery logs (request, response, error). A webhook the resources file owns only shows its
	settings and logs.
-->
<script lang="ts">
	import RelatedLinks from '#lib/components/RelatedLinks.svelte';
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import OutboundWebhookForm from '#lib/admin/OutboundWebhookForm.svelte';
	import { formatDate, relativeTime } from '#lib/batches/batch-change.ts';
	import ManagedNotice from '#lib/components/ManagedNotice.svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import { prettyBody, statusClass } from '#lib/admin/logs.ts';
	import {
		DELETE_OUTBOUND_WEBHOOK,
		OUTBOUND_WEBHOOK,
		UPDATE_OUTBOUND_WEBHOOK,
		describeEventTypes,
		type DeliveryLog,
		type OutboundWebhookDetail,
		type OutboundWebhookResult,
		type ScopedEventType
	} from '#lib/admin/outbound-webhooks.ts';

	const PAGE = 20;

	const id = $derived(page.params.id ?? '');

	let hook = $state.raw<OutboundWebhookDetail | null>(null);
	let logs = $state.raw<DeliveryLog[]>([]);
	let cursor = $state<string | null>(null);
	let hasMore = $state(false);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let notice = $state<string | null>(null);
	let editing = $state(false);
	let onlyErrors = $state(false);

	async function load(more = false): Promise<void> {
		loading = true;
		try {
			const r = await graphql<OutboundWebhookResult, Record<string, unknown>>(OUTBOUND_WEBHOOK, {
				id,
				first: PAGE,
				after: more ? cursor : null,
				onlyErrors
			});
			if (!r.node || r.node.__typename !== 'OutboundWebhook' || !('logs' in r.node)) {
				hook = null;
				error = 'Outbound webhook not found.';
				return;
			}
			hook = r.node;
			logs = more ? [...logs, ...r.node.logs.nodes] : r.node.logs.nodes;
			cursor = r.node.logs.pageInfo.endCursor;
			hasMore = r.node.logs.pageInfo.hasNextPage;
			error = null;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loading = false;
		}
	}

	$effect(() => {
		void [id, onlyErrors];
		void load();
	});

	async function save(input: { url: string; secret: string | null; eventTypes: ScopedEventType[] }): Promise<void> {
		await graphql(UPDATE_OUTBOUND_WEBHOOK, {
			id,
			input: { url: input.url, eventTypes: input.eventTypes, secret: input.secret }
		});
		editing = false;
		notice = 'Saved.';
		await load();
	}

	async function remove(): Promise<void> {
		if (!hook || !confirm(`Delete the webhook to ${hook.url}? Its delivery logs are deleted too.`)) return;
		try {
			await graphql(DELETE_OUTBOUND_WEBHOOK, { id });
			await goto('/site-admin/outbound-webhooks');
		} catch (err) {
			error = errorMessage(err);
		}
	}
</script>

<svelte:head><title>Outbound webhook · Site admin · Sourcegull</title></svelte:head>

<p class="crumbs"><a href="/site-admin/outbound-webhooks">Outbound webhooks</a> /</p>

{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}
{#if notice}<div class="alert alert-info" role="status">{notice}</div>{/if}

{#if hook}
	<div class="page-header">
		<div>
			<h1 class="mono url">{hook.url}</h1>
			<p class="page-description">
				created {formatDate(hook.createdAt)}{hook.createdBy ? ` by ${hook.createdBy.username}` : ''} · updated
				{relativeTime(hook.updatedAt)}{hook.updatedBy ? ` by ${hook.updatedBy.username}` : ''}
			</p>
			<RelatedLinks />
		</div>
		{#if !hook.managedBy}
			<div class="page-actions">
				<button class="btn" type="button" onclick={() => (editing = !editing)}>{editing ? 'Close' : 'Edit'}</button>
				<button class="btn btn-danger" type="button" onclick={remove}>Delete</button>
			</div>
		{/if}
	</div>

	<ManagedNotice managedBy={hook.managedBy} />

	{#if editing && !hook.managedBy}
		<OutboundWebhookForm initial={{ url: hook.url, eventTypes: hook.eventTypes }} submitLabel="Save" onsubmit={save} />
	{:else}
		<dl class="card facts">
			<dt>Event types</dt>
			<dd>{describeEventTypes(hook.eventTypes)}</dd>
			<dt>Deliveries</dt>
			<dd>{hook.stats.total.toLocaleString('en-US')} logged, {hook.stats.errored.toLocaleString('en-US')} failed</dd>
		</dl>
	{/if}

	<section class="section">
		<div class="section-header">
			<h2>Delivery logs</h2>
			<label class="checkbox"><input type="checkbox" bind:checked={onlyErrors} /> Only failed</label>
		</div>
		<div class="table-scroll">
			<table class="table table-bordered table-sm" aria-busy={loading}>
				<thead>
					<tr>
						<th>Sent</th>
						<th>Event</th>
						<th>Status</th>
					</tr>
				</thead>
				<tbody>
					{#each logs as l (l.id)}
						<tr data-testid="delivery-log">
							<td class="small nowrap" title={formatDate(l.sentAt)}>{relativeTime(l.sentAt)}</td>
							<td>
								<details>
									<summary class="mono">{l.job.eventType}{l.job.scope ? ` (${l.job.scope})` : ''}</summary>
									<div class="muted small mono">{l.request.method} {l.request.url}</div>
									{#each l.request.headers as h (h.name)}<div class="mono small">{h.name}: {h.values.join(', ')}</div>{/each}
									<pre class="code">{prettyBody(l.request.body)}</pre>
									{#if l.response}
										<div class="muted small">Response</div>
										<pre class="code">{prettyBody(l.response.body)}</pre>
									{/if}
								</details>
								{#if l.error}<pre class="code bad">{l.error}</pre>{/if}
							</td>
							<td
								><span
									class="badge {statusClass(l.statusCode)}"
									class:badge-success={statusClass(l.statusCode) === 'ok'}
									class:badge-danger={statusClass(l.statusCode) === 'bad'}
									>{l.statusCode || 'no response'}</span
								></td
							>
						</tr>
					{:else}
						<tr><td colspan="3" class="empty-cell">{loading ? 'Loading…' : 'No deliveries logged.'}</td></tr>
					{/each}
				</tbody>
			</table>
		</div>
		{#if hasMore}
			<button class="btn more" type="button" onclick={() => void load(true)} disabled={loading}>Show older</button>
		{/if}
	</section>
{:else if loading}
	<p class="loading" aria-live="polite"><span class="spinner"></span> Loading…</p>
{/if}

<style>
	.crumbs {
		margin: 0 0 var(--space-2);
		font-size: var(--text-sm);
	}
	.url {
		overflow-wrap: anywhere;
		font-size: var(--text-xl);
	}
	.facts {
		display: grid;
		grid-template-columns: max-content minmax(0, 1fr);
		gap: var(--space-2) var(--space-4);
		margin: 0;
		max-width: 48rem;
	}
	.facts dt {
		color: var(--color-text-muted);
	}
	.facts dd {
		margin: 0;
	}
	.more {
		margin-top: var(--space-3);
	}
	.small {
		font-size: var(--text-xs);
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
