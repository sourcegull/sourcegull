<script lang="ts">
	import { onMount } from 'svelte';
	import { page } from '$app/state';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		REPOSITORIES_REFRESH_INTERVAL,
		percent,
		syncInProgress
	} from '#lib/status-messages.ts';
	import {
		ADMIN_REPOSITORIES,
		RECLONE_REPOSITORY,
		REINDEX_REPOSITORY,
		REPOSITORY_FILTERS,
		UPDATE_MIRROR_REPOSITORY,
		cloneState,
		filterVariables,
		formatBytes,
		parseFilter,
		relativeTime,
		type AdminRepositoriesResult,
		type AdminRepositoriesVariables,
		type AdminRepositoryNode,
		type RepositoryFilter,
		type RepositoryStats
	} from '#lib/repos-admin/queries.ts';
	import { NO_CODE_ACCESS_HINT, emptyRepoBadge } from '#lib/repo/queries.ts';

	const PAGE_SIZE = 50;

	let repos = $state.raw<AdminRepositoryNode[]>([]);
	let stats = $state.raw<RepositoryStats | null>(null);
	let totalCount = $state(0);
	let filter = $state<RepositoryFilter>(parseFilter(page.url.searchParams.get('filter')));
	/** Repo nodes low on disk space (clones paused, or over the usage threshold). */
	let diskAlerts = $state.raw<{ message: string; address: string; clonesPaused: boolean }[]>([]);
	let refreshTimer: ReturnType<typeof setTimeout> | undefined;
	let query = $state('');
	/** Cursors of the pages before the current one, and the current page's. */
	let cursors = $state<(string | null)[]>([null]);
	let nextCursor = $state<string | null>(null);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let notice = $state<string | null>(null);
	let pending = $state<string | null>(null);
	let requestSeq = 0;

	/** `quiet`: an automatic refresh, which keeps the current error and notice. */
	async function load(quiet = false): Promise<void> {
		const seq = ++requestSeq;
		clearTimeout(refreshTimer);
		loading = true;
		if (!quiet) error = null;
		try {
			const result = await graphql<AdminRepositoriesResult, AdminRepositoriesVariables>(
				ADMIN_REPOSITORIES,
				{
					first: PAGE_SIZE,
					after: cursors[cursors.length - 1] ?? null,
					query: query.trim() || null,
					...filterVariables(filter)
				}
			);
			if (seq !== requestSeq) return;
			repos = result.repositories.nodes;
			totalCount = result.repositories.totalCount;
			stats = result.repositoryStats;
			diskAlerts = result.statusMessages.flatMap((m) =>
				'message' in m && m.__typename === 'GitserverDiskThresholdReached'
					? [m as { message: string; address: string; clonesPaused: boolean }]
					: []
			);
			// While repositories clone or index, the counts and rows refresh on their own.
			if (syncInProgress(stats)) scheduleRefresh();
			nextCursor = result.repositories.pageInfo.hasNextPage
				? result.repositories.pageInfo.endCursor
				: null;
		} catch (err) {
			if (seq === requestSeq) error = errorMessage(err);
		} finally {
			if (seq === requestSeq) loading = false;
		}
	}

	/** Reloads after the refresh interval; waits while the tab is hidden or an action runs. */
	function scheduleRefresh(): void {
		clearTimeout(refreshTimer);
		refreshTimer = setTimeout(() => {
			if (document.visibilityState === 'visible' && pending === null) void load(true);
			else scheduleRefresh();
		}, REPOSITORIES_REFRESH_INTERVAL);
	}

	function restart(): void {
		cursors = [null];
		void load();
	}

	function showFilter(f: RepositoryFilter): void {
		filter = f;
		restart();
	}

	function nextPage(): void {
		if (!nextCursor) return;
		cursors = [...cursors, nextCursor];
		void load();
	}

	function previousPage(): void {
		if (cursors.length < 2) return;
		cursors = cursors.slice(0, -1);
		void load();
	}

	type Action = 'fetch' | 'reclone' | 'reindex';

	const ACTIONS: Record<Action, { mutation: string; vars: (id: string) => object; done: string }> =
		{
			fetch: {
				mutation: UPDATE_MIRROR_REPOSITORY,
				vars: (id) => ({ repository: id }),
				done: 'fetch queued'
			},
			reclone: {
				mutation: RECLONE_REPOSITORY,
				vars: (id) => ({ repo: id }),
				done: 're-clone queued'
			},
			reindex: {
				mutation: REINDEX_REPOSITORY,
				vars: (id) => ({ repository: id }),
				done: 'reindex queued'
			}
		};

	async function act(repo: AdminRepositoryNode, action: Action): Promise<void> {
		if (
			action === 'reclone' &&
			!confirm(`Clone ${repo.name} again from its code host? The current copy is replaced.`)
		)
			return;
		pending = `${repo.id}:${action}`;
		error = null;
		notice = null;
		try {
			const a = ACTIONS[action];
			await graphql(a.mutation, a.vars(repo.id));
			notice = `${repo.name}: ${a.done}.`;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			pending = null;
		}
	}

	function adminURL(name: string): string {
		return `/site-admin/repositories/${name.split('/').map(encodeURIComponent).join('/')}`;
	}

	const counts = $derived<Partial<Record<RepositoryFilter, number>>>(
		stats
			? {
					all: stats.total,
					cloned: stats.cloned,
					cloning: stats.cloning,
					'not-cloned': stats.notCloned,
					failed: stats.failedFetch,
					indexed: stats.indexed,
					'not-indexed': Math.max(0, stats.total - stats.indexed),
					corrupted: stats.corrupted
				}
			: {}
	);

	const refreshing = $derived(stats !== null && syncInProgress(stats));

	onMount(() => {
		void load();
		return () => clearTimeout(refreshTimer);
	});
</script>

<svelte:head><title>Repositories · Site admin · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Repositories</h1>
		<p class="page-description">
			Clone, fetch and index status of every repository. Disk usage per repo node is on the
			<a href="/site-admin/repo-nodes">repo nodes</a> page.
		</p>
	</div>
	<div class="page-actions">
		{#if stats}
			<span class="badge" title="Size of all clones">{formatBytes(stats.gitDirBytes)} on repo nodes</span>
		{/if}
		<a class="btn btn-secondary" href="/site-admin/external-services">Code host connections</a>
	</div>
</div>

{#each diskAlerts as alert (alert.address)}
	<div class="alert alert-danger" role="alert">
		<strong>{alert.clonesPaused ? 'Clones are paused.' : 'Repo node low on disk space.'}</strong>
		{alert.message} Free disk space or add a repo node: see
		<a href="/site-admin/repo-nodes">repo nodes</a>.
	</div>
{/each}

{#if stats}
	{@const s = stats}
	{@const failedNotCloned = Math.max(0, s.notCloned - s.queued)}
	<section class="summary card" aria-label="Sync status">
		<div class="tiles">
			<button type="button" class="tile" onclick={() => showFilter('cloned')}>
				<span class="value">{s.cloned.toLocaleString('en-US')}</span><span class="tile-label">cloned</span>
			</button>
			<button type="button" class="tile" class:busy={s.cloning > 0} onclick={() => showFilter('cloning')}>
				<span class="value">{s.cloning.toLocaleString('en-US')}</span><span class="tile-label">cloning</span>
			</button>
			<button type="button" class="tile" onclick={() => showFilter('not-cloned')}>
				<span class="value">{s.queued.toLocaleString('en-US')}</span><span class="tile-label">queued</span>
			</button>
			<button type="button" class="tile" class:bad={s.failedFetch > 0} onclick={() => showFilter('failed')}>
				<span class="value">{s.failedFetch.toLocaleString('en-US')}</span><span class="tile-label">failed</span>
			</button>
			<button type="button" class="tile" onclick={() => showFilter('indexed')}>
				<span class="value">{s.indexed.toLocaleString('en-US')}</span><span class="tile-label">indexed</span>
			</button>
		</div>
		<div class="bars">
			<div class="bar-row">
				<span>Cloned {s.cloned.toLocaleString('en-US')} of {s.total.toLocaleString('en-US')}
					{#if failedNotCloned > 0}<span class="error-text">({failedNotCloned.toLocaleString('en-US')} failed to clone)</span>{/if}
					{#if s.noCodeAccess > 0}<span class="badge badge-warning" title={NO_CODE_ACCESS_HINT}
							>{s.noCodeAccess.toLocaleString('en-US')} no code access</span
						>{/if}</span>
				<progress max="100" value={percent(s.cloned, s.total)} aria-label="Cloned"></progress>
				<span class="pct">{percent(s.cloned, s.total)}%</span>
			</div>
			<div class="bar-row">
				<span>Indexed {s.indexed.toLocaleString('en-US')} of {s.cloned.toLocaleString('en-US')} cloned</span>
				<progress max="100" value={percent(s.indexed, s.cloned)} aria-label="Indexed"></progress>
				<span class="pct">{percent(s.indexed, s.cloned)}%</span>
			</div>
		</div>
		<p class="muted small" role="status">
			{#if refreshing}
				Refreshing every {REPOSITORIES_REFRESH_INTERVAL / 1000} s while repositories clone or index.
			{:else}
				Nothing is cloning or indexing.
			{/if}
		</p>
	</section>
{/if}

<div class="toolbar list-toolbar">
<div class="btn-group" role="group" aria-label="Filter repositories">
	{#each REPOSITORY_FILTERS as f (f.value)}
		<button
			type="button"
			class="btn btn-sm"
			class:btn-primary={filter === f.value}
			aria-pressed={filter === f.value}
			onclick={() => showFilter(f.value)}
		>
			{f.label}
			{#if counts[f.value] !== undefined}<span class="count">{counts[f.value]?.toLocaleString('en-US')}</span>{/if}
		</button>
	{/each}
</div>

<form
	class="search"
	onsubmit={(e) => {
		e.preventDefault();
		restart();
	}}
>
	<input
		class="input input-sm"
		type="search"
		placeholder="Search by name"
		aria-label="Search repositories"
		bind:value={query}
	/>
	<button class="btn btn-sm" type="submit">Search</button>
</form>
</div>

{#if error}
	<div class="alert alert-danger" role="alert">{error}</div>
{/if}
{#if notice}
	<div class="alert alert-success" role="status">{notice}</div>
{/if}

<div class="table-scroll">
	<table class="table table-bordered table-sm" aria-busy={loading}>
		<thead>
			<tr>
				<th scope="col">Repository</th>
				<th scope="col">Status</th>
				<th scope="col" class="num">Size</th>
				<th scope="col">Repo node</th>
				<th scope="col">Fetched</th>
				<th scope="col">Changed</th>
				<th scope="col">Indexed</th>
				<th scope="col"><span class="visually-hidden">Actions</span></th>
			</tr>
		</thead>
		<tbody>
			{#each repos as repo (repo.id)}
				{@const m = repo.mirrorInfo}
				{@const state = cloneState(m)}
				<tr>
					<td>
						<a class="mono repo-name" href={adminURL(repo.name)}>{repo.name}</a>
						{#if m.lastError}
							<div class="error small" title={m.lastError}>{m.lastError}</div>
						{/if}
						{#if repo.textSearchIndex?.lastError}
							<div class="error small" title={repo.textSearchIndex.lastError}>
								Index: {repo.textSearchIndex.lastError}
							</div>
						{/if}
					</td>
					<td>
						<span
							class="badge"
							class:badge-success={state === 'cloned'}
							class:badge-info={state === 'cloning'}
							title={m.cloneProgress ?? undefined}>{state}</span
						>
						{#if m.isCorrupted}<span class="badge badge-danger">corrupted</span>{/if}
						{#if m.lastError}<span class="badge badge-danger">fetch failed</span>{/if}
						{#if emptyRepoBadge(m)}
							{@const b = emptyRepoBadge(m)}
							<span class="badge" class:badge-warning={b?.warning} title={b?.title}>{b?.label}</span>
						{/if}
					</td>
					<td class="num">{m.cloned ? formatBytes(m.byteSize) : '—'}</td>
					<td class="mono small nowrap">{m.shard ?? '—'}</td>
					<td title={m.updatedAt ? new Date(m.updatedAt).toLocaleString() : undefined}
						>{relativeTime(m.updatedAt)}</td
					>
					<td title={m.lastChanged ? new Date(m.lastChanged).toLocaleString() : undefined}
						>{relativeTime(m.lastChanged)}</td
					>
					<td
						title={repo.textSearchIndex?.lastIndexedAt
							? new Date(repo.textSearchIndex.lastIndexedAt).toLocaleString()
							: undefined}
					>
						{repo.textSearchIndex?.lastIndexedAt
							? relativeTime(repo.textSearchIndex.lastIndexedAt)
							: 'no'}
					</td>
					<td>
						<div class="actions">
							<button
								class="btn btn-sm"
								type="button"
								disabled={pending !== null}
								onclick={() => act(repo, 'fetch')}>Fetch now</button
							>
							<button
								class="btn btn-sm"
								type="button"
								disabled={pending !== null}
								onclick={() => act(repo, 'reindex')}>Re-index</button
							>
							<button
								class="btn btn-sm btn-danger"
								type="button"
								disabled={pending !== null}
								onclick={() => act(repo, 'reclone')}>Re-clone</button
							>
						</div>
					</td>
				</tr>
			{:else}
				<tr>
					<td colspan="8" class="empty-cell">{loading ? 'Loading…' : 'No repositories match.'}</td>
				</tr>
			{/each}
		</tbody>
	</table>
</div>

<div class="pager">
	<span class="muted">
		{#if totalCount > 0}
			{(cursors.length - 1) * PAGE_SIZE + 1}–{(cursors.length - 1) * PAGE_SIZE + repos.length} of
			{totalCount}
		{/if}
	</span>
	<button class="btn btn-sm" type="button" disabled={cursors.length < 2} onclick={previousPage}
		>Previous</button
	>
	<button class="btn btn-sm" type="button" disabled={!nextCursor} onclick={nextPage}>Next</button>
</div>

<style>
	.summary {
		margin-bottom: var(--space-4);
		padding: var(--space-4) var(--space-5);
	}
	.summary > p {
		margin: var(--space-3) 0 0;
	}
	.tiles {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(7rem, 1fr));
		gap: var(--space-2);
		margin-bottom: var(--space-3);
	}
	.tile {
		display: flex;
		flex-direction: column;
		align-items: flex-start;
		padding: var(--space-2) var(--space-3);
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
		background: var(--color-surface);
		color: var(--color-text);
		font: inherit;
		cursor: pointer;
		text-align: left;
	}
	.tile:hover {
		background: var(--color-bg-hover);
		border-color: var(--color-border-strong);
	}
	.tile .value {
		font-size: var(--text-xl);
		font-weight: var(--weight-semibold);
		font-variant-numeric: tabular-nums;
	}
	.tile .tile-label {
		color: var(--color-text-muted);
	}
	.tile.busy .value {
		color: var(--color-primary);
	}
	.tile.bad .value {
		color: var(--color-danger);
	}
	.bars {
		display: grid;
		gap: var(--space-1);
		max-width: 48rem;
	}
	.bar-row {
		display: grid;
		grid-template-columns: minmax(0, 1fr) minmax(6rem, 14rem) 3rem;
		align-items: center;
		gap: var(--space-2);
	}
	.bar-row progress {
		width: 100%;
	}
	.pct {
		text-align: right;
		font-variant-numeric: tabular-nums;
	}
		
.list-toolbar {
		justify-content: space-between;
		margin-bottom: var(--space-3);
	}
	.search {
		display: flex;
		gap: var(--space-2);
		flex: 1 1 16rem;
		justify-content: flex-end;
	}
	.search input {
		max-width: 24rem;
	}
		.error {
		color: var(--color-danger);
		max-width: 32rem;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
		.badge + .badge {
		margin-left: var(--space-1);
	}
	.actions {
		display: flex;
		flex-wrap: wrap;
		justify-content: flex-end;
		gap: var(--space-1);
		min-width: 9rem;
	}
	.repo-name,
	.nowrap {
		white-space: nowrap;
	}
	.pager {
		display: flex;
		align-items: center;
		justify-content: flex-end;
		gap: var(--space-2);
		margin-top: var(--space-3);
	}
</style>
