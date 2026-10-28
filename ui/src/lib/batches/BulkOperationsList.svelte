<!--
	The bulk operations of a bulk change, newest first: what, who, when, progress while
	processing, and the changesets an operation failed on with their errors. Refreshes while one
	is processing.
-->
<script lang="ts">
	import { onDestroy } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		BULK_OPERATIONS,
		bulkOperationSummary,
		formatDate,
		relativeTime,
		type BulkOperationNode,
		type BulkOperationsResult
	} from './batch-change.ts';
	import StateBadge from './StateBadge.svelte';

	let { batchChangeId }: { batchChangeId: string } = $props();

	let nodes = $state.raw<BulkOperationNode[]>([]);
	let totalCount = $state(0);
	let first = $state(20);
	let hasNext = $state(false);
	let loading = $state(true);
	let error = $state<string | null>(null);

	async function load(): Promise<void> {
		try {
			const r = await graphql<BulkOperationsResult, { batchChange: string; first: number }>(BULK_OPERATIONS, {
				batchChange: batchChangeId,
				first
			});
			const conn = r.node?.bulkOperations;
			nodes = conn?.nodes ?? [];
			totalCount = conn?.totalCount ?? 0;
			hasNext = conn?.pageInfo.hasNextPage ?? false;
			error = null;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loading = false;
		}
	}

	$effect(() => {
		void first;
		void load();
	});

	let poll: ReturnType<typeof setTimeout> | undefined;
	$effect(() => {
		clearTimeout(poll);
		if (nodes.some((n) => n.state === 'PROCESSING')) poll = setTimeout(() => void load(), 3000);
	});
	onDestroy(() => clearTimeout(poll));

	const tone = (s: string) => (s === 'COMPLETED' ? 'success' : s === 'FAILED' ? 'danger' : 'info');
</script>

{#if error}
	<div class="alert alert-danger" role="alert">{error}</div>
{/if}

<ul class="operations" aria-busy={loading} aria-label="Bulk operations">
	{#each nodes as op (op.id)}
		<li class="operation card" data-bulk-operation={op.type}>
			<div class="head">
				<StateBadge state={op.state} tone={tone(op.state)} />
				<strong>{bulkOperationSummary(op.type, op.changesetCount)}</strong>
				<span class="muted small">
					by {op.initiator.username}, <span title={formatDate(op.createdAt)}>{relativeTime(op.createdAt)}</span>
				</span>
			</div>
			{#if op.state === 'PROCESSING'}
				<div
					class="meter"
					role="progressbar"
					aria-label="Progress"
					aria-valuemin="0"
					aria-valuemax="100"
					aria-valuenow={Math.round(op.progress * 100)}
				>
					<div class="fill" style:width="{op.progress * 100}%"></div>
				</div>
				<span class="muted small">{Math.round(op.progress * 100)}% done</span>
			{:else if op.finishedAt}
				<span class="muted small">Finished {relativeTime(op.finishedAt)}</span>
			{/if}
			{#if op.errors.length > 0}
				<details class="errors">
					<summary>{op.errors.length} {op.errors.length === 1 ? 'error' : 'errors'}</summary>
					<ul>
						{#each op.errors as e, i (i)}
							<li>
								{#if e.changeset.__typename === 'ExternalChangeset'}
									<strong>{e.changeset.title ?? 'Changeset'}</strong>
									<span class="muted">({e.changeset.repository.name})</span>
								{:else}
									<span class="muted">A changeset in a repository you don't have access to</span>
								{/if}
								{#if e.error}<pre class="error">{e.error}</pre>{/if}
							</li>
						{/each}
					</ul>
				</details>
			{/if}
		</li>
	{:else}
		{#if !loading}
			<li class="empty muted">No bulk operations yet.</li>
		{/if}
	{/each}
</ul>
<div class="footer">
	<span class="muted small">{totalCount} {totalCount === 1 ? 'bulk operation' : 'bulk operations'}</span>
	{#if hasNext}
		<button class="btn btn-sm" type="button" onclick={() => (first += 20)}>Show more</button>
	{/if}
</div>

<style>
	.operations {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
	}
	.operation {
		padding: var(--space-3) var(--space-4);
		display: flex;
		flex-direction: column;
		gap: var(--space-1);
	}
	.head {
		display: flex;
		gap: var(--space-2);
		align-items: baseline;
		flex-wrap: wrap;
	}
	.meter {
		height: 6px;
		border-radius: 3px;
		background: var(--color-info-bg);
		overflow: hidden;
	}
	.fill {
		height: 100%;
		background: var(--color-primary);
		border-radius: 3px;
	}
	.errors ul {
		margin: var(--space-2) 0 0;
		padding-left: var(--space-4);
	}
	.error {
		color: var(--color-danger);
		white-space: pre-wrap;
		overflow-wrap: anywhere;
		margin: var(--space-1) 0;
		font-size: 0.85em;
	}
	.empty {
		padding: var(--space-6) 0;
		text-align: center;
	}
	.footer {
		display: flex;
		justify-content: space-between;
		align-items: center;
		margin-top: var(--space-3);
	}
	.small {
		font-size: 0.875em;
	}
</style>
