<!--
	Code host connections: every connection with its repositories and sync status, "sync
	now", and the way to add one. Site admins only (the site-admin layout checks). Connections
	the resources file owns can be synced, not edited; when it owns them all, none can be added.
-->
<script lang="ts">
	import RelatedLinks from '#lib/components/RelatedLinks.svelte';
	import { onMount } from 'svelte';
	import { formatDate, relativeTime } from '#lib/batches/batch-change.ts';
	import ManagedBadge from '#lib/components/ManagedBadge.svelte';
	import ManagedNotice from '#lib/components/ManagedNotice.svelte';
	import {
		EXTERNAL_SERVICES,
		SYNC_EXTERNAL_SERVICE,
		kindLabel,
		type ExternalServiceSummary,
		type ExternalServicesResult
	} from '#lib/connections/queries.ts';
	import { isExclusive } from '#lib/declared-config.ts';
	import { errorMessage, graphql } from '#lib/graphql.ts';

	const PAGE = 50;

	let services = $state.raw<ExternalServiceSummary[]>([]);
	let totalCount = $state(0);
	let endCursor = $state<string | null>(null);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let notice = $state<string | null>(null);
	let exclusive = $state(false);

	async function load(after: string | null = null): Promise<void> {
		loading = true;
		error = null;
		try {
			const r = await graphql<ExternalServicesResult, { first: number; after: string | null }>(
				EXTERNAL_SERVICES,
				{ first: PAGE, after }
			);
			services = after ? [...services, ...r.externalServices.nodes] : r.externalServices.nodes;
			totalCount = r.externalServices.totalCount;
			exclusive = isExclusive(r.site.declaredConfigFiles, 'connections');
			endCursor = r.externalServices.pageInfo.hasNextPage ? r.externalServices.pageInfo.endCursor : null;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loading = false;
		}
	}

	async function syncNow(s: ExternalServiceSummary): Promise<void> {
		error = null;
		notice = null;
		try {
			await graphql(SYNC_EXTERNAL_SERVICE, { id: s.id });
			notice = `A sync of ${s.displayName} was queued.`;
		} catch (err) {
			error = errorMessage(err);
		}
	}

	onMount(() => {
		void load();
	});
</script>

<svelte:head><title>Code host connections · Site admin · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Code host connections</h1>
		<p class="page-description">
			Each connection mirrors the repositories its configuration lists from a GitLab, GitHub or
			Gitea instance and keeps them in sync.
		</p>
		<RelatedLinks />
	</div>
	{#if !exclusive}
		<div class="page-actions">
			<a class="btn btn-primary" href="/site-admin/external-services/new">Add connection</a>
		</div>
	{/if}
</div>

{#if exclusive}<ManagedNotice kind="connections" />{/if}

{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}
{#if notice}<div class="alert alert-success" role="status">{notice}</div>{/if}

<div class="table-scroll">
	<table class="table table-bordered connections">
		<thead>
			<tr>
				<th>Connection</th>
				<th class="num">Repositories</th>
				<th>Last sync</th>
				<th>Next sync</th>
				<th><span class="visually-hidden">Actions</span></th>
			</tr>
		</thead>
		<tbody>
			{#each services as s (s.id)}
				<tr data-connection={s.displayName}>
					<td>
						<a href="/site-admin/external-services/{encodeURIComponent(s.id)}"><strong>{s.displayName}</strong></a>
						<ManagedBadge managedBy={s.managedBy} />
						<div class="muted small">{kindLabel(s.kind)} · {s.url}</div>
					</td>
					<td class="num">{s.repoCount.toLocaleString('en-US')}</td>
					<td>
						{#if s.lastSyncAt}
							<span title={formatDate(s.lastSyncAt)}>{relativeTime(s.lastSyncAt)}</span>
						{:else}
							<span class="muted">never</span>
						{/if}
						{#if s.lastSyncError}
							<div><span class="badge badge-danger" title={s.lastSyncError}>failed</span></div>
						{/if}
					</td>
					<td>
						{#if s.nextSyncAt}
							<span title={formatDate(s.nextSyncAt)}>{relativeTime(s.nextSyncAt)}</span>
						{:else}
							<span class="muted">—</span>
						{/if}
					</td>
					<td class="actions">
						<button class="btn btn-sm" type="button" onclick={() => syncNow(s)}>Sync now</button>
						{#if !s.managedBy}
							<a class="btn btn-sm" href="/site-admin/external-services/{encodeURIComponent(s.id)}/edit">Edit</a>
						{/if}
					</td>
				</tr>
			{:else}
				{#if !loading && exclusive}
					<tr>
						<td colspan="5" class="empty-cell">
							No code host connections yet. Declare them in the <code class="code-inline">connections</code>
							section of SOURCEGULL_RESOURCES_FILE.
						</td>
					</tr>
				{:else if !loading}
					<tr>
						<td colspan="5" class="empty-cell">
							No code host connections yet. Add a
							<a href="/site-admin/external-services/new?kind=gitlab">GitLab</a>,
							<a href="/site-admin/external-services/new?kind=github">GitHub</a> or
							<a href="/site-admin/external-services/new?kind=gitea">Gitea</a> connection to start mirroring
							repositories.
						</td>
					</tr>
				{/if}
			{/each}
		</tbody>
	</table>
</div>

<div class="list-footer">
	{#if loading}
		<p class="loading" aria-live="polite"><span class="spinner"></span> Loading…</p>
	{:else if endCursor}
		<button class="btn" type="button" onclick={() => load(endCursor)}>Show more</button>
	{/if}
	{#if totalCount > 0}
		<p class="muted small">{totalCount} connection{totalCount === 1 ? '' : 's'}</p>
	{/if}
</div>

<style>
	.actions {
		white-space: nowrap;
		text-align: right;
	}
	.actions > * + * {
		margin-left: var(--space-1);
	}
	.list-footer {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--space-3);
		margin-top: var(--space-3);
	}
	.list-footer p {
		margin: 0;
	}
</style>
