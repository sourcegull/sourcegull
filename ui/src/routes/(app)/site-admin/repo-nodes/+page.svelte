<script lang="ts">
	import RelatedLinks from '#lib/components/RelatedLinks.svelte';
	import type { RetryFailedRepoMovesMutation } from '#lib/graphql-types.ts';
	import { onMount } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		GITSERVERS,
		REPO_REBALANCE,
		RETRY_FAILED_REPO_MOVES,
		formatBytes,
		rebalanceSummary,
		relativeTime,
		type GitserverNode,
		type GitserversResult,
		type RepoRebalance,
		type RepoRebalanceResult
	} from '#lib/repos-admin/queries.ts';

	let nodes = $state.raw<GitserverNode[]>([]);
	let rebalance = $state.raw<RepoRebalance | null>(null);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let rebalanceError = $state<string | null>(null);
	let retrying = $state(false);

	async function loadRebalance(): Promise<void> {
		rebalanceError = null;
		try {
			rebalance = (await graphql<RepoRebalanceResult>(REPO_REBALANCE)).repoRebalance;
		} catch (err) {
			rebalanceError = errorMessage(err);
		}
	}

	async function load(): Promise<void> {
		loading = true;
		error = null;
		const moves = loadRebalance();
		try {
			nodes = (await graphql<GitserversResult>(GITSERVERS)).gitservers.nodes;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			await moves;
			loading = false;
		}
	}

	async function retryFailed(): Promise<void> {
		retrying = true;
		try {
			await graphql<RetryFailedRepoMovesMutation>(RETRY_FAILED_REPO_MOVES);
			await loadRebalance();
		} catch (err) {
			rebalanceError = errorMessage(err);
		} finally {
			retrying = false;
		}
	}

	/** Percent of the volume in use, or null when unknown. */
	function usedPercent(n: GitserverNode): number | null {
		const total = Number(n.totalDiskSpaceBytes);
		if (!total) return null;
		return Math.round(((total - Number(n.freeDiskSpaceBytes)) / total) * 100);
	}

	onMount(() => void load());
</script>

<svelte:head><title>Repo nodes · Site admin · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Repo nodes</h1>
		<p class="page-description">
			Repo nodes store the repositories and their search index shards. Repository sizes are as last
			measured after a fetch; disk space and shard sizes are read from each node now.
		</p>
		<RelatedLinks />
	</div>
	<div class="page-actions">
		<button class="btn btn-secondary" type="button" disabled={loading} onclick={() => void load()}
			>Refresh</button
		>
	</div>
</div>

{#if error}
	<div class="alert alert-danger" role="alert">{error}</div>
{/if}

<div class="table-scroll">
	<table class="table table-bordered" aria-busy={loading}>
		<thead>
			<tr>
				<th scope="col">Node</th>
				<th scope="col">Disk</th>
				<th scope="col" class="num">Repositories</th>
				<th scope="col" class="num">Used by repositories</th>
				<th scope="col" class="num">Index</th>
			</tr>
		</thead>
		<tbody>
			{#each nodes as n (n.id)}
				{@const used = usedPercent(n)}
				<tr>
					<td>
						<strong class="mono">{n.address}</strong>
						{#if !n.configured}
							<div><span class="badge badge-warning">not in the node list</span></div>
						{/if}
						{#if n.error}
							<div class="error small">Unreachable: {n.error}</div>
						{/if}
					</td>
					<td>
						{#if used !== null}
							<div
								class="meter"
								role="meter"
								aria-label="Disk used on {n.address}"
								aria-valuemin={0}
								aria-valuemax={100}
								aria-valuenow={used}
							>
								<span style:width="{used}%" class:high={used >= 90}></span>
							</div>
							<div class="small">
								{formatBytes(n.freeDiskSpaceBytes)} free of {formatBytes(n.totalDiskSpaceBytes)} ({used}%
								used)
							</div>
							{#if n.clonePauseThresholdBytes !== null}
								<div class="muted small">
									Clones pause below {formatBytes(n.clonePauseThresholdBytes)} free
								</div>
							{/if}
							{#if n.clonesPaused}
								<div><span class="badge badge-warning">clones paused: low disk space</span></div>
							{/if}
						{:else}
							<span class="muted">unknown</span>
						{/if}
					</td>
					<td class="num">
						{n.repositoriesCount.toLocaleString()} held
						<div class="muted small">
							{n.assignedRepositoriesCount.toLocaleString()} assigned
							{#if n.repositoriesOnDisk !== null}
								<br />{n.repositoriesOnDisk.toLocaleString()} on disk
							{/if}
						</div>
					</td>
					<td class="num">{formatBytes(n.repositoriesDiskBytes)}</td>
					<td class="num">
						{n.indexDiskBytes !== null ? formatBytes(n.indexDiskBytes) : '—'}
						<div class="muted small">
							{n.indexedRepositoriesCount.toLocaleString()} repositories indexed
						</div>
					</td>
				</tr>
			{:else}
				<tr>
					<td colspan="5" class="empty-cell">{loading ? 'Loading…' : 'No repo nodes configured.'}</td>
				</tr>
			{/each}
		</tbody>
	</table>
</div>

<section class="rebalance" aria-labelledby="rebalance-heading">
	<div class="section-header">
		<h2 id="rebalance-heading">Rebalancing</h2>
		{#if rebalance && rebalance.failed > 0}
			<button
				class="btn btn-sm"
				type="button"
				disabled={retrying}
				onclick={() => void retryFailed()}>Retry failed moves</button
			>
		{/if}
	</div>
	<p class="muted">
		When repo nodes are added or removed, each repository whose node changes is copied from its old
		node, checked, switched over, and removed from the old node a few minutes later. Searches and
		git requests keep working during a move.
	</p>
	{#if rebalanceError}
		<div class="alert alert-danger" role="alert">{rebalanceError}</div>
	{/if}
	{#if rebalance}
		<p data-testid="rebalance-summary">
			<strong>{rebalanceSummary(rebalance)}</strong>
			{#if rebalance.done > 0}
				<span class="muted small">{rebalance.done.toLocaleString()} moved in the last week.</span>
			{/if}
		</p>
		{#if rebalance.moves.length > 0}
			<div class="table-scroll">
				<table class="table table-bordered table-sm">
					<thead>
						<tr>
							<th scope="col">Repository</th>
							<th scope="col">From → to</th>
							<th scope="col">State</th>
							<th scope="col">Updated</th>
						</tr>
					</thead>
					<tbody>
						{#each rebalance.moves as m (m.repositoryId)}
							<tr>
								<td>
									<a href="/site-admin/repositories/{m.repositoryName}">{m.repositoryName}</a>
								</td>
								<td class="mono small">
									{m.fromNode}<br />→ {m.toNode}
								</td>
								<td>
									<span
										class="badge"
										class:badge-danger={m.state === 'failed'}
										class:badge-success={m.state === 'done'}
										class:badge-info={m.state === 'copying' || m.state === 'flipped'}
									>{m.state}</span>
									{#if m.kind === 'reclone'}
										<div class="muted small">old node gone: cloning from the code host</div>
									{:else if Number(m.bytesCopied) > 0}
										<div class="muted small">{formatBytes(m.bytesCopied)} copied</div>
									{/if}
									{#if m.attempts > 1}
										<div class="muted small">attempt {m.attempts}</div>
									{/if}
									{#if m.lastError}
										<div class="error small">{m.lastError}</div>
									{/if}
									{#if m.state === 'queued' && m.nextAttemptAt}
										<div class="muted small">retry {relativeTime(m.nextAttemptAt)}</div>
									{/if}
								</td>
								<td class="small">{relativeTime(m.updatedAt)}</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		{/if}
	{/if}
</section>

<style>
	.rebalance > p {
		max-width: 48rem;
	}
	.error {
		color: var(--color-danger);
	}
	td.num .muted {
		white-space: normal;
	}
	.meter {
		width: 10rem;
		height: 0.5rem;
		border-radius: var(--radius-full);
		background: var(--color-bg-muted);
		overflow: hidden;
		margin-bottom: var(--space-1);
	}
	.meter span {
		display: block;
		height: 100%;
		background: var(--color-primary);
	}
	.meter span.high {
		background: var(--color-danger);
	}
</style>
