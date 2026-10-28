<!--
	The execution of a server-side bulk change spec: overall state and progress, cancel and retry, the workspaces with their state, one
	workspace in detail, the spec as it ran, and the way to the preview once it completed. Polls
	while the execution is going.
-->
<script lang="ts">
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { onDestroy, onMount } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import WorkspaceDetail from './WorkspaceDetail.svelte';
	import {
		BATCH_SPEC_EXECUTION,
		CANCEL_BATCH_SPEC_EXECUTION,
		RETRY_BATCH_SPEC_EXECUTION,
		canCancel,
		canModifyExecution,
		elapsed,
		executionProgress,
		formatDuration,
		isExecutionActive,
		isWorkspaceActive,
		queuePosition,
		stateLabel,
		stateTone,
		workspaceTitle,
		type BatchSpecExecutionResult,
		type ExecutionSpec,
		type WorkspaceState
	} from './execution.ts';
	import { batchChangeURLFromRoute, editURL, loadBatchesAccess, type BatchesAccess, type NamespaceKind } from './spec.ts';

	interface Props {
		kind: NamespaceKind;
		namespace: string;
		name: string;
		specID: string;
	}

	let { kind, namespace, name, specID }: Props = $props();

	const POLL_MS = 2000;
	const PAGE = 100;
	const STATES: WorkspaceState[] = ['QUEUED', 'PROCESSING', 'COMPLETED', 'FAILED', 'CANCELED', 'SKIPPED', 'PENDING'];

	let access = $state.raw<BatchesAccess | null>(null);
	let spec = $state.raw<ExecutionSpec | null>(null);
	let notFound = $state(false);
	let loadError = $state<string | null>(null);
	let actionError = $state<string | null>(null);
	let busy = $state(false);
	let search = $state('');
	let stateFilter = $state<WorkspaceState | ''>('');
	let now = $state(Date.now());
	let timer: ReturnType<typeof setTimeout> | undefined;
	let destroyed = false;
	let seq = 0;

	const selected = $derived(page.url.searchParams.get('workspace'));
	const bcURL = $derived(batchChangeURLFromRoute(kind, namespace, name));
	const viewer = $derived(access?.available ? access.viewer : null);
	const canModify = $derived(spec !== null && canModifyExecution(spec, viewer));
	const resolution = $derived(spec?.workspaceResolution ?? null);
	const progress = $derived(
		resolution ? executionProgress(resolution.all.stats, resolution.all.totalCount, resolution.skipped.totalCount) : null
	);
	const workspaces = $derived(resolution?.workspaces.nodes ?? []);
	const selectedState = $derived(workspaces.find((w) => w.id === selected)?.state);

	async function load(): Promise<void> {
		const mine = ++seq;
		clearTimeout(timer);
		try {
			if (access === null) access = await loadBatchesAccess();
			if (!access.available) return;
			const r = await graphql<
				BatchSpecExecutionResult,
				{ id: string; first: number; search: string | null; state: WorkspaceState | null }
			>(BATCH_SPEC_EXECUTION, { id: specID, first: PAGE, search: search.trim() || null, state: stateFilter || null });
			if (mine !== seq) return;
			loadError = null;
			if (r.node?.__typename !== 'BatchSpec') {
				notFound = true;
				return;
			}
			spec = r.node as ExecutionSpec;
			now = Date.now();
		} catch (err) {
			if (mine === seq) loadError = errorMessage(err);
		}
		if (mine !== seq || destroyed || !spec) return;
		const active = isExecutionActive(spec.state) || workspaces.some((w) => isWorkspaceActive(w.state));
		if (active) timer = setTimeout(() => void load(), POLL_MS);
	}

	onMount(() => void load());
	onDestroy(() => {
		destroyed = true;
		clearTimeout(timer);
	});

	let debounce: ReturnType<typeof setTimeout> | undefined;
	function onsearch(): void {
		clearTimeout(debounce);
		debounce = setTimeout(() => void load(), 250);
	}

	function select(id: string): void {
		const url = new URL(page.url.href);
		url.searchParams.set('workspace', id);
		void goto(url, { replace: true, reset: false });
	}

	async function act(f: () => Promise<unknown>): Promise<void> {
		busy = true;
		actionError = null;
		try {
			await f();
			await load();
		} catch (err) {
			actionError = errorMessage(err);
		} finally {
			busy = false;
		}
	}

	const cancel = () => {
		if (!spec || !confirm('Cancel the execution? Running steps stop and queued workspaces do not run.')) return;
		const id = spec.id;
		void act(() => graphql(CANCEL_BATCH_SPEC_EXECUTION, { batchSpec: id }));
	};
	const retry = () => {
		if (!spec) return;
		const id = spec.id;
		void act(() => graphql(RETRY_BATCH_SPEC_EXECUTION, { batchSpec: id, includeCompleted: false }));
	};

	const time = (iso: string | null) => (iso ? new Date(iso).toLocaleString() : '—');
	/** Percentage of the workspaces that run, for the progress bar's segments. */
	const share = (n: number) => (progress && progress.total > 0 ? Math.round((100 * Math.min(n, progress.total)) / progress.total) : 0);
</script>

<svelte:head><title>Execution · {name} · Bulk changes · Sourcegull</title></svelte:head>

<div class="exec-page">
	<nav class="crumbs muted" aria-label="Breadcrumbs">
		<a href="/bulk-changes">Bulk changes</a> / {namespace} / <a href={bcURL}>{name}</a> / Execution
	</nav>
	<div class="page-header">
		<h1>{name}</h1>
		{#if spec}
			<span class="badge tone-{stateTone(spec.state)}" data-testid="spec-state">{stateLabel(spec.state)}</span>
		{/if}
		<span class="spacer"></span>
		{#if spec}
			<div class="toolbar">
				<a class="btn" href={editURL(bcURL)}>Edit spec</a>
				{#if canCancel(spec, viewer)}
					<button class="btn btn-danger" type="button" disabled={busy} onclick={cancel}>Cancel</button>
				{/if}
				{#if spec.viewerCanRetry}
					<button class="btn" type="button" disabled={busy} onclick={retry}>Retry failed workspaces</button>
				{/if}
				{#if spec.state === 'COMPLETED' && spec.applyURL}
					<a class="btn btn-primary" href={spec.applyURL} data-testid="preview-link">Preview changesets</a>
				{/if}
			</div>
		{/if}
	</div>

	{#if loadError}
		<div class="alert alert-danger" role="alert">{loadError}</div>
	{/if}
	{#if access && !access.available}
		<div class="alert alert-warning" role="alert">{access.message}</div>
	{:else if notFound}
		<div class="alert alert-warning" role="alert">Bulk change spec not found.</div>
	{:else if !spec}
		{#if !loadError}<p class="muted">Loading…</p>{/if}
	{:else}
		<div class="summary">
			<div class="meta muted small">
				{#if spec.creator}Run by <strong>{spec.creator.username}</strong> ·{/if}
				started {time(spec.startedAt)}
				{#if elapsed(spec.startedAt, spec.finishedAt, now) !== null}
					· {spec.finishedAt ? 'took' : 'running for'} {formatDuration(elapsed(spec.startedAt, spec.finishedAt, now) ?? 0)}
				{/if}
				{#if spec.noCache}· cache disabled{/if}
				{#if spec.diffStat}
					· <span class="added">+{spec.diffStat.added}</span> <span class="deleted">−{spec.diffStat.deleted}</span>
				{/if}
			</div>
			{#if progress && resolution}
				<div
					class="progress"
					role="progressbar"
					aria-label="Workspaces finished"
					aria-valuemin="0"
					aria-valuemax={progress.total}
					aria-valuenow={progress.done}
				>
					<div class="bar ok" style:width="{share(resolution.all.stats.completed)}%"></div>
					<div class="bar failed" style:width="{share(resolution.all.stats.errored)}%"></div>
				</div>
				<div class="counts small" data-testid="execution-counts">
					<span>{progress.done} of {progress.total} workspaces finished</span>
					<span class="added">{resolution.all.stats.completed} completed</span>
					{#if resolution.all.stats.errored > 0}<span class="deleted">{resolution.all.stats.errored} failed</span>{/if}
					{#if resolution.all.stats.processing > 0}<span>{resolution.all.stats.processing} running</span>{/if}
					{#if resolution.all.stats.queued > 0}<span>{resolution.all.stats.queued} queued</span>{/if}
					{#if resolution.skipped.totalCount > 0}<span class="muted">{resolution.skipped.totalCount} skipped</span>{/if}
				</div>
			{/if}
		</div>

		{#if actionError}<div class="alert alert-danger" role="alert">{actionError}</div>{/if}
		{#if spec.failureMessage}
			<div class="alert alert-danger failure" role="alert" data-testid="spec-failure">{spec.failureMessage}</div>
		{/if}
		{#if spec.state === 'COMPLETED' && spec.applyURL}
			<div class="alert alert-success" role="status">
				The execution completed. <a href={spec.applyURL}>Preview the changesets</a> to apply the bulk change.
			</div>
		{/if}

		<div class="layout">
			<section class="list" aria-label="Workspaces">
				<div class="filters">
					<input
						class="input"
						type="search"
						placeholder="Filter by repository…"
						aria-label="Filter workspaces"
						bind:value={search}
						oninput={onsearch}
					/>
					<select class="input" aria-label="Workspace state" bind:value={stateFilter} onchange={() => void load()}>
						<option value="">All states</option>
						{#each STATES as s (s)}<option value={s}>{stateLabel(s)}</option>{/each}
					</select>
				</div>
				<ul class="workspaces">
					{#each workspaces as ws (ws.id)}
						<li>
							<button
								type="button"
								class="ws"
								class:active={ws.id === selected}
								aria-current={ws.id === selected ? 'true' : undefined}
								data-repo={ws.repository?.name ?? ''}
								onclick={() => select(ws.id)}
							>
								<span class="ws-line">
									<span class="dot tone-{stateTone(ws.state)}" aria-hidden="true"></span>
									<span class="ws-name mono">{workspaceTitle(ws)}</span>
								</span>
								<span class="ws-meta small muted">
									<span class="state">{stateLabel(ws.state)}</span>
									{#if queuePosition(ws)}· {queuePosition(ws)}{/if}
									{#if ws.cachedResultFound}· cached{/if}
									{#if ws.diffStat}
										· <span class="added">+{ws.diffStat.added}</span> <span class="deleted">−{ws.diffStat.deleted}</span>
									{/if}
									{#if elapsed(ws.startedAt, ws.finishedAt, now) !== null}
										· {formatDuration(elapsed(ws.startedAt, ws.finishedAt, now) ?? 0)}
									{/if}
								</span>
							</button>
						</li>
					{:else}
						<li class="muted empty">No workspaces.</li>
					{/each}
				</ul>
				{#if resolution && resolution.workspaces.totalCount > workspaces.length}
					<p class="muted small">Showing {workspaces.length} of {resolution.workspaces.totalCount}; filter to find others.</p>
				{/if}
			</section>
			<div class="pane">
				{#if selected}
					<WorkspaceDetail id={selected} {canModify} listState={selectedState} onchange={() => void load()} />
				{:else}
					<p class="muted">Select a workspace to see its steps and logs.</p>
				{/if}
				<details class="spec-input">
					<summary>Bulk change spec</summary>
					<pre>{spec.originalInput}</pre>
				</details>
			</div>
		</div>
	{/if}
</div>

<style>
	.exec-page {
		max-width: 96rem;
		margin: 0 auto;
		padding: var(--space-6) var(--space-4);
	}
	.crumbs {
		margin-bottom: var(--space-2);
		overflow-wrap: anywhere;
	}
	.page-header {
		flex-wrap: wrap;
		justify-content: flex-start;
	}
	.page-header h1 {
		overflow-wrap: anywhere;
	}
	.spacer {
		flex: 1;
	}
	.small {
		font-size: 0.875em;
	}
	.summary {
		margin-bottom: var(--space-4);
	}
	.meta {
		margin-bottom: var(--space-2);
	}
	.progress {
		display: flex;
		height: 0.5rem;
		border-radius: 999px;
		background: var(--color-bg-muted);
		overflow: hidden;
	}
	.bar {
		height: 100%;
	}
	.bar.ok {
		background: var(--color-success);
	}
	.bar.failed {
		background: var(--color-danger);
	}
	.counts {
		display: flex;
		flex-wrap: wrap;
		gap: var(--space-3);
		margin-top: var(--space-1);
	}
	.failure {
		white-space: pre-wrap;
		overflow-wrap: anywhere;
	}
	.added {
		color: var(--color-success);
	}
	.deleted {
		color: var(--color-danger);
	}
	.layout {
		display: grid;
		grid-template-columns: minmax(0, 2fr) minmax(0, 3fr);
		gap: var(--space-6);
		align-items: start;
	}
	.filters {
		display: flex;
		gap: var(--space-2);
		margin-bottom: var(--space-2);
	}
	.filters select {
		width: auto;
	}
	.workspaces {
		list-style: none;
		margin: 0 0 var(--space-2);
		padding: 0;
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
		max-height: 70vh;
		overflow-y: auto;
	}
	.workspaces li + li {
		border-top: 1px solid var(--color-border);
	}
	.ws {
		display: flex;
		flex-direction: column;
		gap: 2px;
		width: 100%;
		text-align: left;
		padding: var(--space-2) var(--space-3);
		border: 0;
		background: none;
		color: inherit;
		font: inherit;
		cursor: pointer;
	}
	.ws:hover {
		background: var(--color-bg-subtle);
	}
	.ws.active {
		background: var(--color-info-bg);
	}
	.ws-line {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		min-width: 0;
	}
	.ws-name {
		overflow-wrap: anywhere;
		font-weight: 600;
	}
	.empty {
		padding: var(--space-3);
	}
	.dot {
		flex: none;
		width: 0.55rem;
		height: 0.55rem;
		border-radius: 50%;
		background: var(--color-border-strong);
	}
	.dot.tone-success {
		background: var(--color-success);
	}
	.dot.tone-danger {
		background: var(--color-danger);
	}
	.dot.tone-info {
		background: var(--color-primary);
	}
	.dot.tone-warning {
		background: var(--color-warning);
	}
	.badge.tone-success {
		background: var(--color-success-bg);
		color: var(--color-success);
	}
	.badge.tone-danger {
		background: var(--color-danger-bg);
		color: var(--color-danger);
	}
	.badge.tone-info {
		background: var(--color-info-bg);
		color: var(--color-primary);
	}
	.badge.tone-warning {
		background: var(--color-warning-bg);
		color: var(--color-warning);
	}
	.pane {
		min-width: 0;
	}
	.spec-input {
		margin-top: var(--space-4);
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
	}
	.spec-input summary {
		cursor: pointer;
		padding: var(--space-2) var(--space-3);
	}
	.spec-input pre {
		margin: 0;
		padding: var(--space-3);
		max-height: 30rem;
		overflow: auto;
		background: var(--color-bg-subtle);
		white-space: pre-wrap;
		overflow-wrap: anywhere;
	}
	@media (max-width: 60rem) {
		.layout {
			grid-template-columns: minmax(0, 1fr);
		}
	}
</style>
