<!--
	A bulk change, at `BatchChange.url`: header with state and actions (edit spec,
	close with or without its changesets, delete), the description, changeset stats, and the
	tabs Changesets, Burndown chart, Spec, Archived and Bulk operations (`?tab=`).
-->
<script lang="ts">
	import type { BatchChangeOrgIdQuery, BatchChangeUserIdQuery } from '#lib/graphql-types.ts';
	import { logEvent } from '#lib/events.ts';
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import BulkOperationsList from '#lib/batches/BulkOperationsList.svelte';
	import BurndownChart from '#lib/batches/BurndownChart.svelte';
	import ChangesetList from '#lib/batches/ChangesetList.svelte';
	import Markdown from '#lib/batches/Markdown.svelte';
	import StateBadge from '#lib/batches/StateBadge.svelte';
	import {
		BATCH_CHANGE,
		CLOSE_BATCH_CHANGE,
		DELETE_BATCH_CHANGE,
		DETAIL_TABS,
		ORG_ID,
		USER_ID,
		detailTab,
		editSpecURL,
		formatDate,
		namespaceBatchChangesURL,
		relativeTime,
		statsEntries,
		type BatchChangeDetail,
		type BatchChangeResult,
		type NamespaceKind
	} from '#lib/batches/batch-change.ts';

	const kind = $derived((page.params.nskind ?? 'users') as NamespaceKind);
	const ns = $derived(page.params.ns ?? '');
	const name = $derived(page.params.name ?? '');
	const tab = $derived(detailTab(page.url.searchParams.get('tab')));

	let bc = $state.raw<BatchChangeDetail | null>(null);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let actionError = $state<string | null>(null);
	let closing = $state(false);
	let closeChangesets = $state(false);
	let busy = $state(false);

	async function load(): Promise<void> {
		error = null;
		try {
			const idQuery = kind === 'users' ? USER_ID : ORG_ID;
			const r = await graphql<Partial<BatchChangeUserIdQuery & BatchChangeOrgIdQuery>, { name: string }>(
				idQuery,
				{ name: ns }
			);
			const nsId = r.user?.id ?? r.organization?.id;
			if (!nsId) {
				error = `No ${kind === 'users' ? 'user' : 'organization'} named ${ns}.`;
				bc = null;
				return;
			}
			const d = await graphql<BatchChangeResult, { namespace: string; name: string }>(BATCH_CHANGE, {
				namespace: nsId,
				name
			});
			bc = d.batchChange;
			if (!bc) error = 'Bulk change not found.';
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loading = false;
		}
	}

	$effect(() => {
		void kind;
		void ns;
		void name;
		loading = true;
		void load();
	});

	function tabURL(id: string): string {
		const url = new URL(page.url.href);
		if (id === 'changesets') url.searchParams.delete('tab');
		else url.searchParams.set('tab', id);
		return url.pathname + url.search;
	}

	async function close(event: SubmitEvent): Promise<void> {
		event.preventDefault();
		if (!bc) return;
		busy = true;
		actionError = null;
		try {
			await graphql(CLOSE_BATCH_CHANGE, { batchChange: bc.id, closeChangesets });
			closing = false;
			await load();
		} catch (err) {
			actionError = errorMessage(err);
		} finally {
			busy = false;
		}
	}

	async function remove(): Promise<void> {
		if (!bc) return;
		if (!confirm(`Delete the bulk change ${bc.name}? Its changesets stay on the code host.`)) return;
		busy = true;
		actionError = null;
		try {
			await graphql(DELETE_BATCH_CHANGE, { batchChange: bc.id });
			await goto('/bulk-changes');
		} catch (err) {
			actionError = errorMessage(err);
			busy = false;
		}
	}

	const stateTone = (s: string) => (s === 'OPEN' ? 'success' : s === 'CLOSED' ? 'danger' : 'muted');
	const openCount = $derived(bc ? bc.changesetsStats.open + bc.changesetsStats.draft : 0);

	// A usage event for the admin analytics.
	$effect(() => {
		if (page.params.name) logEvent('BatchChangeDetailsPageViewed');
	});
</script>

<svelte:head><title>{name} · Bulk changes · Sourcegull</title></svelte:head>

<div class="page">
	<nav class="crumbs" aria-label="Breadcrumb">
		<a href="/bulk-changes">Bulk changes</a> /
		<a href={namespaceBatchChangesURL(kind, ns)}>{ns}</a> /
	</nav>

	{#if error}
		<div class="alert alert-danger" role="alert">{error}</div>
	{:else if loading && !bc}
		<p class="loading"><span class="spinner"></span>Loading…</p>
	{/if}

	{#if bc}
		<div class="page-header">
			<div class="title-block">
				<div class="heading">
					<h1>{bc.name}</h1>
					<StateBadge state={bc.state} tone={stateTone(bc.state)} />
				</div>
				<p class="page-description meta">
					{#if bc.creator}Created by <a href={bc.creator.url}>{bc.creator.username}</a>{:else}Created{/if}
					<span title={formatDate(bc.createdAt)}>{relativeTime(bc.createdAt)}</span>
					{#if bc.lastAppliedAt && bc.lastApplier}
						· last applied by <a href={bc.lastApplier.url}>{bc.lastApplier.username}</a>
						<span title={formatDate(bc.lastAppliedAt)}>{relativeTime(bc.lastAppliedAt)}</span>
					{/if}
					{#if bc.closedAt}
						· closed <span title={formatDate(bc.closedAt)}>{relativeTime(bc.closedAt)}</span>
					{/if}
				</p>
			</div>
			{#if bc.viewerCanAdminister}
				<div class="page-actions">
					<a class="btn" href={editSpecURL(bc.url)}>Edit spec</a>
					{#if bc.state !== 'CLOSED'}
						<button class="btn" type="button" disabled={busy} onclick={() => (closing = !closing)}>Close</button>
					{/if}
					<button class="btn btn-danger" type="button" disabled={busy} onclick={remove}>Delete</button>
				</div>
			{/if}
		</div>

		{#if actionError}
			<div class="alert alert-danger" role="alert">{actionError}</div>
		{/if}

		{#if closing}
			<form class="card close-form" onsubmit={close} aria-label="Close bulk change">
				<h2>Close bulk change</h2>
				<p>
					Closing the bulk change stops updating its changesets. It can't be reopened, but a new
					spec can be applied to a bulk change of the same name.
				</p>
				<label class="checkbox">
					<input type="checkbox" bind:checked={closeChangesets} />
					Also close all {openCount} open {openCount === 1 ? 'changeset' : 'changesets'} on the code host
				</label>
				<div class="toolbar">
					<button class="btn btn-danger" type="submit" disabled={busy}>Close bulk change</button>
					<button class="btn" type="button" onclick={() => (closing = false)}>Cancel</button>
				</div>
			</form>
		{/if}

		{#if bc.description}
			<div class="description"><Markdown source={bc.description} /></div>
		{/if}

		{@const s = bc.changesetsStats}
		<section class="stats card" aria-label="Changeset stats">
			<div class="progress">
				<div class="percent"><strong>{s.percentComplete}%</strong> <span class="muted">complete</span></div>
				<div
					class="meter"
					role="progressbar"
					aria-label="Merged or closed"
					aria-valuemin="0"
					aria-valuemax="100"
					aria-valuenow={s.percentComplete}
				>
					<div class="fill" style:width="{s.percentComplete}%"></div>
				</div>
				<div class="muted small">
					<span class="added">+{bc.diffStat.added}</span>
					<span class="deleted">−{bc.diffStat.deleted}</span>
				</div>
			</div>
			<dl class="counts">
				{#each statsEntries(s) as e (e.key)}
					<div data-stat={e.key}><dt>{e.label}</dt><dd>{e.count}</dd></div>
				{/each}
				<div data-stat="total"><dt>Total</dt><dd>{s.total}</dd></div>
			</dl>
		</section>

		{#if s.scheduled > 0}
			<div class="alert alert-info" role="note">
				{s.scheduled} {s.scheduled === 1 ? 'changeset is' : 'changesets are'} waiting for a rollout window
				(<code>batchChanges.rolloutWindows</code>); the list shows when each is expected to be published.
			</div>
		{/if}

		<nav class="tabs" aria-label="Bulk change">
			{#each DETAIL_TABS as t (t.id)}
				<a href={tabURL(t.id)} aria-current={tab === t.id ? 'page' : undefined} data-sveltekit-noscroll>
					{t.title}
					{#if t.id === 'changesets'}<span class="badge">{s.total - s.archived}</span>{/if}
					{#if t.id === 'archived'}<span class="badge">{s.archived}</span>{/if}
					{#if t.id === 'bulkoperations'}<span class="badge">{bc.bulkOperations.totalCount}</span>{/if}
				</a>
			{/each}
		</nav>

		<div class="tab-panel">
			{#if tab === 'changesets'}
				<ChangesetList batchChange={bc} onbulk={load} />
			{:else if tab === 'archived'}
				<ChangesetList batchChange={bc} archived onbulk={load} />
			{:else if tab === 'chart'}
				<BurndownChart batchChangeId={bc.id} />
			{:else if tab === 'spec'}
				<div class="spec-header">
					<span class="muted small">
						{bc.currentSpec.source === 'LOCAL' ? 'Uploaded from a local run' : 'Run on the server'},
						<span title={formatDate(bc.currentSpec.createdAt)}>{relativeTime(bc.currentSpec.createdAt)}</span>
					</span>
					{#if bc.viewerCanAdminister}
						<a class="btn btn-sm" href={editSpecURL(bc.url)}>Edit spec</a>
					{/if}
				</div>
				<pre class="spec mono" aria-label="Bulk change spec">{bc.currentSpec.originalInput}</pre>
			{:else}
				<BulkOperationsList batchChangeId={bc.id} />
			{/if}
		</div>
	{/if}
</div>

<style>
	.page {
		max-width: 72rem;
	}
	.title-block {
		min-width: 0;
	}
	.heading {
		display: flex;
		align-items: center;
		gap: var(--space-3);
		min-width: 0;
	}
	.heading h1 {
		overflow-wrap: anywhere;
	}
	.close-form {
		margin-bottom: var(--space-4);
		display: flex;
		flex-direction: column;
		gap: var(--space-3);
		padding: var(--space-4);
	}
	.close-form h2,
	.close-form p {
		margin: 0;
	}
	.description {
		margin-bottom: var(--space-4);
		max-width: 52rem;
	}
	.stats {
		display: flex;
		gap: var(--space-6);
		align-items: center;
		flex-wrap: wrap;
		padding: var(--space-4);
		margin-bottom: var(--space-4);
	}
	.progress {
		flex: 1 1 14rem;
		display: flex;
		flex-direction: column;
		gap: var(--space-1);
	}
	.percent strong {
		font-size: var(--text-2xl);
		font-weight: var(--weight-semibold);
	}
	.meter {
		height: 8px;
		border-radius: var(--radius-full);
		background: var(--color-bg-muted);
		overflow: hidden;
	}
	.fill {
		height: 100%;
		background: var(--series-merged);
		border-radius: var(--radius-full);
	}
	.counts {
		display: flex;
		gap: var(--space-4);
		flex-wrap: wrap;
		margin: 0;
	}
	.counts div {
		text-align: center;
		min-width: 3.5rem;
	}
	.counts dt {
		display: inline-flex;
		align-items: center;
		gap: var(--space-1);
		font-size: var(--text-xs);
		color: var(--color-text-muted);
	}
	.counts [data-stat='open'] dt::before,
	.counts [data-stat='merged'] dt::before,
	.counts [data-stat='closed'] dt::before,
	.counts [data-stat='draft'] dt::before {
		content: '';
		width: 8px;
		height: 8px;
		border-radius: 50%;
		background: var(--dot);
	}
	.counts [data-stat='open'] {
		--dot: var(--series-open);
	}
	.counts [data-stat='merged'] {
		--dot: var(--series-merged);
	}
	.counts [data-stat='closed'] {
		--dot: var(--series-closed);
	}
	.counts [data-stat='draft'] {
		--dot: var(--series-draft);
	}
	.counts dd {
		margin: 0;
		font-size: var(--text-lg);
		font-weight: 600;
		font-variant-numeric: tabular-nums;
	}
	.added {
		color: var(--color-success);
		font-weight: 600;
	}
	.deleted {
		color: var(--color-danger);
		font-weight: 600;
	}
	.spec-header {
		display: flex;
		justify-content: space-between;
		align-items: center;
		gap: var(--space-3);
		margin-bottom: var(--space-2);
	}
	.spec {
		background: var(--color-bg-subtle);
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
		padding: var(--space-3);
		overflow-x: auto;
		font-size: var(--text-sm);
		margin: 0;
	}
</style>
