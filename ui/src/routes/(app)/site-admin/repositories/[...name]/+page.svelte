<script lang="ts">
	import RelatedLinks from '#lib/components/RelatedLinks.svelte';
	import { page } from '$app/state';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import { emptyRepoMessage } from '#lib/repo/queries.ts';
	import {
		ADMIN_REPOSITORY,
		RECLONE_REPOSITORY,
		REINDEX_REPOSITORY,
		UPDATE_MIRROR_REPOSITORY,
		cloneState,
		formatBytes,
		relativeTime,
		remoteWebURL,
		type AdminRepository,
		type AdminRepositoryResult
	} from '#lib/repos-admin/queries.ts';

	const name = $derived(page.params.name ?? '');

	let repo = $state.raw<AdminRepository | null>(null);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let notice = $state<string | null>(null);
	let busy = $state(false);

	async function load(repoName: string): Promise<void> {
		loading = true;
		error = null;
		try {
			const r = await graphql<AdminRepositoryResult, { name: string }>(ADMIN_REPOSITORY, {
				name: repoName
			});
			repo = r.repository;
			if (!repo) error = 'Repository not found.';
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loading = false;
		}
	}

	async function run(mutation: string, vars: object, done: string): Promise<void> {
		busy = true;
		error = null;
		notice = null;
		try {
			await graphql(mutation, vars);
			notice = done;
			await load(name);
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = false;
		}
	}

	function reclone(): void {
		if (!repo) return;
		if (!confirm(`Clone ${repo.name} again from its code host? The current copy is replaced.`))
			return;
		void run(RECLONE_REPOSITORY, { repo: repo.id }, 'Re-clone queued.');
	}

	function when(iso: string | null | undefined): string {
		return iso ? `${new Date(iso).toLocaleString()} (${relativeTime(iso)})` : '—';
	}

	function short(oid: string | null | undefined): string {
		return oid ? oid.slice(0, 10) : '—';
	}

	$effect(() => {
		void load(name);
	});
</script>

<svelte:head><title>{name} · Repositories · Site admin · Sourcegull</title></svelte:head>

<p class="back"><a href="/site-admin/repositories">← Repositories</a></p>

<div class="page-header">
	<div class="title">
		<h1 class="mono name">{name}</h1>
		<p class="page-description">Mirror and text search index of this repository on its repo node.</p>
		<RelatedLinks />
	</div>
	{#if repo}
		<div class="page-actions">
			<a class="btn btn-sm" href={repo.url}>Browse</a>
			<button
				class="btn btn-sm"
				type="button"
				disabled={busy}
				onclick={() =>
					repo && run(UPDATE_MIRROR_REPOSITORY, { repository: repo.id }, 'Fetch queued.')}
				>Fetch now</button
			>
			<button
				class="btn btn-sm"
				type="button"
				disabled={busy}
				onclick={() =>
					repo &&
					run(
						REINDEX_REPOSITORY,
						{ repository: repo.id },
						'Reindex queued on the repo node.'
					)}>Re-index</button
			>
			<button class="btn btn-sm btn-danger" type="button" disabled={busy} onclick={reclone}
				>Re-clone</button
			>
		</div>
	{/if}
</div>

{#if error}
	<div class="alert alert-danger" role="alert">{error}</div>
{/if}
{#if notice}
	<div class="alert alert-success" role="status">{notice}</div>
{/if}
{#if loading && !repo}
	<p class="loading"><span class="spinner"></span> Loading…</p>
{/if}

{#if repo}
	{@const m = repo.mirrorInfo}
	{@const ix = repo.textSearchIndex}
	<section class="card">
		<div class="card-header"><h2>Mirror</h2></div>
		<dl class="facts">
			<dt>Status</dt>
			<dd>
				<span class="badge" class:badge-success={cloneState(m) === 'cloned'}>{cloneState(m)}</span>
				{#if m.isCorrupted}<span class="badge badge-danger">corrupted</span>{/if}
				{#if m.isEmpty}<span class="badge">empty</span>{/if}
				{#if m.cloneProgress}<span class="muted mono small">{m.cloneProgress}</span>{/if}
			</dd>
			<dt>Repo node</dt>
			<dd class="mono">{m.shard ?? '—'}</dd>
			<dt>Size on disk</dt>
			<dd>{m.cloned ? formatBytes(m.byteSize) : '—'}</dd>
			<dt>Last fetched</dt>
			<dd>{when(m.updatedAt)}</dd>
			<dt>Last changed</dt>
			<dd>{when(m.lastChanged)}</dd>
			<dt>Next fetch</dt>
			<dd>{when(m.nextSyncAt)}</dd>
			<dt>Remote</dt>
			<dd class="mono small">
				{#if remoteWebURL(m.remoteURL)}
					<a href={remoteWebURL(m.remoteURL)} target="_blank" rel="noopener noreferrer">{m.remoteURL}</a>
				{:else}
					{m.remoteURL || '—'}
				{/if}
			</dd>
			<dt>Code host connections</dt>
			<dd>
				{repo.externalServices.nodes.map((s) => s.displayName).join(', ') || '—'}
			</dd>
		</dl>
		{#if emptyRepoMessage(m)}
			{@const noAccess = m.remoteUnavailable === 'NO_CODE_ACCESS'}
			<div class="alert" class:alert-info={!noAccess} class:alert-warning={noAccess} role="status">
				{emptyRepoMessage(m)}
				{noAccess || m.remoteUnavailable === 'DISABLED'
					? 'The repo node fetches it once it becomes available.'
					: 'The repo node picks up the first push on its next fetch.'}
			</div>
		{/if}
		{#if m.lastError}
			<h3>Last fetch error</h3>
			<pre class="log">{m.lastError}</pre>
		{/if}
		{#each m.corruptionLogs as log (log.timestamp)}
			<div class="alert alert-warning" role="status">
				Found corrupt {when(log.timestamp)}: {log.reason}. The repo node re-clones it.
			</div>
		{/each}
	</section>

	<section class="card">
		<div class="card-header"><h2>Text search index</h2></div>
		{#if ix}
			{#if ix.nodeError}
				<div class="alert alert-warning" role="status">
					The repo node could not be asked for the live index status: {ix.nodeError}
				</div>
			{/if}
			<dl class="facts">
				<dt>Repo node</dt>
				<dd class="mono">{ix.host?.name ?? '—'}</dd>
				<dt>Status</dt>
				<dd>
					{#if ix.status}<span class="badge badge-success">indexed</span>{:else}<span class="badge"
							>not indexed</span
						>{/if}
					{#if ix.queued}<span class="badge badge-info">build queued</span>{/if}
				</dd>
				<dt>Last indexed</dt>
				<dd>{when(ix.status?.updatedAt ?? ix.lastIndexedAt)}</dd>
				{#if ix.status}
					<dt>Files</dt>
					<dd>
						{ix.status.contentFilesCount.toLocaleString()} ({formatBytes(
							ix.status.contentByteSize
						)} of text)
					</dd>
					<dt>Lines</dt>
					<dd>
						{ix.status.newLinesCount.toLocaleString()}
						<span class="muted"
							>({ix.status.defaultBranchNewLinesCount.toLocaleString()} on the default branch)</span
						>
					</dd>
					<dt>Index size</dt>
					<dd>{formatBytes(ix.status.indexByteSize)} in {ix.status.indexShardsCount} file(s)</dd>
				{/if}
			</dl>
			{#if ix.lastError}
				<h3>Last index error <span class="muted small">{when(ix.lastErrorAt)}</span></h3>
				<pre class="log">{ix.lastError}</pre>
			{/if}

			<h3>Branches</h3>
			<div class="table-scroll">
				<table class="table table-bordered table-sm">
					<thead>
						<tr>
							<th scope="col">Branch</th>
							<th scope="col">Indexed commit</th>
							<th scope="col">Head</th>
							<th scope="col">State</th>
						</tr>
					</thead>
					<tbody>
						{#each ix.refs as r (r.indexName)}
							<tr>
								<td class="mono">
									{r.ref.displayName || r.indexName}
									{#if r.indexName === 'HEAD'}<span class="muted small">(default)</span>{/if}
								</td>
								<td class="mono">{short(r.indexedCommit?.oid)}</td>
								<td class="mono">{short(r.ref.target.oid)}</td>
								<td>
									{#if !r.indexed}
										<span class="badge">not indexed</span>
									{:else if r.current}
										<span class="badge badge-success">current</span>
									{:else}
										<span class="badge badge-warning">stale</span>
									{/if}
								</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>

			{#if ix.shards.length > 0}
				<h3>Shards</h3>
				<div class="table-scroll">
					<table class="table table-bordered table-sm">
						<thead>
							<tr>
								<th scope="col">File</th>
								<th scope="col">Kind</th>
								<th scope="col" class="num">Size</th>
								<th scope="col" class="num">Files of this repository</th>
								<th scope="col" class="num">Repositories</th>
							</tr>
						</thead>
						<tbody>
							{#each ix.shards as s (s.file)}
								<tr>
									<td class="mono">{s.file}</td>
									<td>{s.kind}</td>
									<td class="num">{formatBytes(s.byteSize)}</td>
									<td class="num">{s.filesCount.toLocaleString()}</td>
									<td class="num">{s.repositoriesCount}</td>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			{/if}
		{/if}
	</section>
{/if}

<style>
	.back {
		margin: 0 0 var(--space-2);
		font-size: var(--text-sm);
	}
	.title {
		min-width: 0;
	}
	.name {
		font-size: var(--text-xl);
		word-break: break-all;
	}
	section {
		margin-bottom: var(--space-6);
	}
	section h3 {
		font-size: var(--text-md);
		margin: var(--space-5) 0 var(--space-2);
	}
	.facts {
		display: grid;
		grid-template-columns: max-content 1fr;
		gap: var(--space-2) var(--space-6);
		margin: 0 0 var(--space-4);
	}
	.facts dt {
		color: var(--color-text-muted);
		font-weight: var(--weight-medium);
	}
	.facts dd {
		margin: 0;
		min-width: 0;
		overflow-wrap: anywhere;
	}
	.small {
		font-size: var(--text-sm);
	}
	.log {
		white-space: pre-wrap;
		font-family: var(--font-mono);
		font-size: var(--text-sm);
		background: var(--color-danger-bg);
		border: 1px solid var(--color-danger-border);
		border-radius: var(--radius);
		padding: var(--space-3);
		color: var(--color-danger);
	}
	.badge + .badge {
		margin-left: var(--space-1);
	}
	@media (max-width: 40rem) {
		.facts {
			grid-template-columns: minmax(0, 1fr);
			gap: 0 0;
		}
		.facts dd {
			margin-bottom: var(--space-2);
		}
	}
</style>
