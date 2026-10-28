<script lang="ts">
	import { loadAuthState } from '#lib/auth.ts';
	import PoliciesView from '#lib/codeintel/PoliciesView.svelte';
	import RetentionPanel from '#lib/codeintel/RetentionPanel.svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import { relativeDate } from './blame.ts';
	import { formatBytes } from './diff.ts';
	import IndexConfigurationView from './IndexConfigurationView.svelte';
	import IndexJobLogs from './IndexJobLogs.svelte';
	import {
		canDelete,
		canReindex,
		DELETE_PRECISE_INDEX,
		displayRoot,
		PRECISE_INDEXES,
		REINDEX_PRECISE_INDEX,
		STATE_FILTERS,
		stateLabel,
		type PreciseIndexesResult,
		type PreciseIndexesVariables,
		type PreciseIndexNode
	} from './codegraph.ts';

	interface Props {
		/** The repository's node ID. */
		repoId: string;
	}

	let { repoId }: Props = $props();

	const PAGE = 50;

	let nodes = $state.raw<PreciseIndexNode[]>([]);
	let total = $state(0);
	let endCursor = $state<string | null>(null);
	let indexerKeys = $state.raw<string[]>([]);
	let query = $state('');
	let stateFilter = $state(0);
	let indexerKey = $state('');
	let loading = $state(true);
	let error = $state<string | null>(null);
	let notice = $state<string | null>(null);
	let busy = $state<string | null>(null);
	let siteAdmin = $state(false);
	let tab = $state<'indexes' | 'policies' | 'configuration'>('indexes');
	/** The index whose retention is shown. */
	let retentionOf = $state<string | null>(null);
	/** The index job whose steps and logs are shown. */
	let logsOf = $state<string | null>(null);
	let seq = 0;

	loadAuthState()
		.then((s) => (siteAdmin = s.user?.siteAdmin === true))
		.catch(() => {});

	$effect(() => {
		void load(repoId, false);
	});

	async function load(repo: string, more: boolean): Promise<void> {
		const mine = ++seq;
		loading = true;
		error = null;
		try {
			const r = await graphql<PreciseIndexesResult, PreciseIndexesVariables>(PRECISE_INDEXES, {
				repo,
				query: query.trim() || null,
				states: STATE_FILTERS[stateFilter]?.states ?? null,
				indexerKey: indexerKey || null,
				first: PAGE,
				after: more ? endCursor : null
			});
			if (mine !== seq) return;
			const c = r.preciseIndexes;
			nodes = more ? [...nodes, ...c.nodes] : c.nodes;
			total = c.totalCount ?? nodes.length;
			endCursor = c.pageInfo.hasNextPage ? c.pageInfo.endCursor : null;
			indexerKeys = r.indexerKeys;
		} catch (err) {
			if (mine === seq) error = errorMessage(err);
		} finally {
			if (mine === seq) loading = false;
		}
	}

	let debounce: ReturnType<typeof setTimeout> | undefined;
	function oninput(): void {
		clearTimeout(debounce);
		debounce = setTimeout(() => load(repoId, false), 200);
	}

	async function act(n: PreciseIndexNode, action: 'delete' | 'reindex'): Promise<void> {
		const what = `the index of ${displayRoot(n.inputRoot)} at ${n.inputCommit.slice(0, 7)}`;
		if (action === 'delete' && !confirm(`Delete ${what}? Precise navigation from it stops at once.`)) {
			return;
		}
		busy = n.id;
		error = null;
		notice = null;
		try {
			await graphql(action === 'delete' ? DELETE_PRECISE_INDEX : REINDEX_PRECISE_INDEX, { id: n.id });
			notice =
				action === 'delete'
					? `Deleting ${what}.`
					: n.steps
						? `Queued ${what} for indexing again.`
						: `Queued ${what} for processing again.`;
			await load(repoId, false);
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = null;
		}
	}
</script>

<div class="code-graph">
	<div class="subtabs" role="tablist">
		<button role="tab" class="tab" aria-selected={tab === 'indexes'} onclick={() => (tab = 'indexes')}>
			Precise indexes
		</button>
		<button role="tab" class="tab" aria-selected={tab === 'policies'} onclick={() => (tab = 'policies')}>
			Configuration policies
		</button>
		<button
			role="tab"
			class="tab"
			aria-selected={tab === 'configuration'}
			onclick={() => (tab = 'configuration')}
		>
			Index configuration
		</button>
	</div>
	{#if tab === 'policies'}
		<PoliciesView {repoId} />
	{:else if tab === 'configuration'}
		<IndexConfigurationView {repoId} {siteAdmin} onqueued={() => load(repoId, false)} />
	{:else}
		<div class="page-header">
			<h2>Code graph data</h2>
			<span class="muted">{total} precise {total === 1 ? 'index' : 'indexes'}</span>
		</div>
		<p class="muted intro">
			SCIP indexes uploaded for this repository (for example by <code>src code-intel upload</code> in CI) and
			auto-indexing jobs run by executors. Completed indexes give precise code navigation at their commit.
		</p>

		<div class="filters">
			<label class="visually-hidden" for="index-filter">Filter indexes</label>
			<input
				id="index-filter"
				class="input"
				type="search"
				placeholder="Filter by commit, root or indexer…"
				bind:value={query}
				{oninput}
			/>
			<label class="visually-hidden" for="index-state">State</label>
			<select id="index-state" class="input" bind:value={stateFilter} onchange={() => load(repoId, false)}>
				{#each STATE_FILTERS as f, i (f.label)}
					<option value={i}>{f.label}</option>
				{/each}
			</select>
			{#if indexerKeys.length > 1}
				<label class="visually-hidden" for="index-indexer">Indexer</label>
				<select id="index-indexer" class="input" bind:value={indexerKey} onchange={() => load(repoId, false)}>
					<option value="">All indexers</option>
					{#each indexerKeys as k (k)}
						<option value={k}>{k}</option>
					{/each}
				</select>
			{/if}
		</div>

		{#if error}
			<div class="alert alert-danger" role="alert">{error}</div>
		{/if}
		{#if notice}
			<div class="alert alert-info" role="status">{notice}</div>
		{/if}

		<div class="table-scroll">
			<table class="table" aria-busy={loading}>
				<thead>
					<tr>
						<th>Root</th>
						<th>Indexer</th>
						<th>Commit</th>
						<th>State</th>
						<th>Uploaded or queued</th>
						<th class="num">Size</th>
						{#if siteAdmin}<th><span class="visually-hidden">Actions</span></th>{/if}
					</tr>
				</thead>
				<tbody>
					{#each nodes as n (n.id)}
						{@const s = stateLabel(n.state)}
						<tr data-root={n.inputRoot}>
							<td class="mono">
								{#if n.projectRoot}
									<a href={n.projectRoot.url}>{displayRoot(n.inputRoot)}</a>
								{:else}
									{displayRoot(n.inputRoot)}
								{/if}
							</td>
							<td>
								{#if n.indexer?.url}
									<a href={n.indexer.url} target="_blank" rel="noopener noreferrer">{n.indexer.name}</a>
								{:else}
									{n.indexer?.name ?? n.inputIndexer}
								{/if}
							</td>
							<td class="mono">
								{#if n.projectRoot}
									<a href={n.projectRoot.commit.url}>{n.projectRoot.commit.abbreviatedOID}</a>
								{:else}
									{n.inputCommit.slice(0, 7)}
								{/if}
							</td>
							<td class="nowrap">
								<span class="badge state-{s.tone}">{s.text}</span>
								{#if n.expired && n.state === 'COMPLETED'}<span class="badge state-muted" title="No retention policy keeps it">expired</span>{/if}
								{#if n.placeInQueue !== null}<span class="muted">#{n.placeInQueue}</span>{/if}
								{#if n.shouldReindex}<span class="badge state-muted" title="A new index job replaces it">reindexing</span>{/if}
							</td>
							<td class="muted nowrap" title={n.uploadedAt ?? n.queuedAt ?? ''}>
								{#if n.uploadedAt}{relativeDate(n.uploadedAt)}{:else if n.queuedAt}queued {relativeDate(n.queuedAt)}{/if}
							</td>
							<td class="muted num nowrap">{n.uploadSize === null ? '' : formatBytes(n.uploadSize)}</td>
							{#if siteAdmin}
								<td class="actions nowrap">
									{#if canReindex(n.state)}
										<button class="btn btn-sm" disabled={busy !== null} onclick={() => act(n, 'reindex')}>
											Reindex
										</button>
									{/if}
									{#if canDelete(n.state)}
										<button class="btn btn-sm btn-danger" disabled={busy !== null} onclick={() => act(n, 'delete')}>
											Delete
										</button>
									{/if}
								</td>
							{/if}
						</tr>
						{#if n.failure || n.numDocuments !== null || n.state === 'COMPLETED' || n.steps || n.gitlabCiJob}
							<tr class="detail">
								<td colspan={siteAdmin ? 7 : 6}>
									{#if n.failure}
										<pre class="failure" class:retrying={n.state !== 'PROCESSING_ERRORED'}>{n.failure}</pre>
									{/if}
									{#if n.gitlabCiJob}
										<span class="muted">
											Uploaded by GitLab CI job {n.gitlabCiJob.jobId} on {n.gitlabCiJob.ref}{#if n.gitlabCiJob.username}
												(triggered by {n.gitlabCiJob.username}){/if}
										</span>
									{/if}
									{#if n.numDocuments !== null && n.state === 'COMPLETED'}
										<span class="muted">
											{n.numDocuments} documents · {n.numOccurrences ?? 0} occurrences
											{#if n.processingFinishedAt}· processed {relativeDate(n.processingFinishedAt)}{/if}
										</span>
									{/if}
									{#if n.state === 'COMPLETED'}
										<button
											class="link-button"
											aria-expanded={retentionOf === n.id}
											onclick={() => (retentionOf = retentionOf === n.id ? null : n.id)}
										>
											{retentionOf === n.id ? 'Hide retention' : 'Retention'}
										</button>
										{#if retentionOf === n.id}<RetentionPanel indexId={n.id} />{/if}
									{/if}
									{#if n.steps}
										<button
											class="link-button"
											aria-expanded={logsOf === n.id}
											onclick={() => (logsOf = logsOf === n.id ? null : n.id)}
										>
											{logsOf === n.id ? 'Hide index job' : 'Index job logs'}
										</button>
										{#if logsOf === n.id}<IndexJobLogs steps={n.steps} />{/if}
									{/if}
								</td>
							</tr>
						{/if}
					{:else}
						<tr>
							<td colspan={siteAdmin ? 7 : 6} class="muted">
								{loading ? 'Loading…' : 'No precise indexes.'}
							</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
		{#if endCursor}
			<button class="btn more" disabled={loading} onclick={() => load(repoId, true)}>Show more</button>
		{/if}
	{/if}
</div>

<style>
	.code-graph {
		padding: var(--space-4);
		max-width: 80rem;
	}
	.subtabs {
		display: flex;
		gap: var(--space-1);
		border-bottom: 1px solid var(--color-border);
		margin-bottom: var(--space-3);
	}
	.tab {
		background: none;
		border: none;
		border-bottom: 2px solid transparent;
		padding: var(--space-2) var(--space-3);
		color: var(--color-text-muted);
		cursor: pointer;
		font: inherit;
	}
	.tab[aria-selected='true'] {
		border-bottom-color: var(--color-primary);
		color: inherit;
		font-weight: 600;
	}
	.link-button {
		background: none;
		border: none;
		padding: 0;
		margin-left: var(--space-2);
		color: var(--color-primary);
		cursor: pointer;
		font: inherit;
		font-size: 0.9em;
	}
	.state-muted {
		background: var(--color-warning-bg);
		color: var(--color-warning);
	}
	.intro {
		margin-top: 0;
	}
	.filters {
		display: flex;
		flex-wrap: wrap;
		gap: var(--space-2);
		margin-bottom: var(--space-3);
	}
	.filters input {
		flex: 1 1 16rem;
	}
	.filters select {
		flex: 0 0 auto;
		width: auto;
	}
	.nowrap {
		white-space: nowrap;
	}
	.num {
		text-align: right;
	}
	.actions {
		text-align: right;
	}
	.actions .btn + .btn {
		margin-left: var(--space-1);
	}
	.state-success {
		background: var(--color-success-bg);
		color: var(--color-success);
	}
	.state-danger {
		background: var(--color-danger-bg);
		color: var(--color-danger);
	}
	.state-info {
		background: var(--color-info-bg);
		color: var(--color-primary);
	}
	.detail td {
		padding-top: 0;
	}
	tr:has(+ .detail) td {
		border-bottom: none;
	}
	.failure {
		margin: 0 0 var(--space-1);
		padding: var(--space-2);
		border-radius: var(--radius);
		background: var(--color-danger-bg);
		color: var(--color-danger);
		white-space: pre-wrap;
		overflow-wrap: anywhere;
		font-size: 0.85em;
	}
	.failure.retrying {
		background: var(--color-warning-bg);
		color: var(--color-warning);
	}
	.more {
		margin-top: var(--space-3);
	}
</style>
