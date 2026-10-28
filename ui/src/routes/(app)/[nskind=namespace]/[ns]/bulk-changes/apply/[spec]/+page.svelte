<!--
	Preview and apply of a bulk change spec, at `BatchSpec.applyURL`: what
	applying does to each changeset (`applyPreview`: operations, what changes, filters, stats),
	publication states chosen per changeset where the spec leaves `published` unset, and Apply
	(`applyBatchChange` with `ensureBatchChange`) or Create (`createBatchChange`).
-->
<script lang="ts">
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { SvelteMap, SvelteSet } from 'svelte/reactivity';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import DiffView from '#lib/repo/DiffView.svelte';
	import Markdown from '#lib/batches/Markdown.svelte';
	import StateBadge from '#lib/batches/StateBadge.svelte';
	import {
		formatDate,
		namespaceBatchChangesURL,
		namespaceName,
		relativeTime,
		type NamespaceKind
	} from '#lib/batches/batch-change.ts';
	import {
		CHANGESET_SPEC_DIFF,
		CHANGESET_STATES,
		enumLabel,
		stateTone,
		toFileDiffConnection,
		type ChangesetSpecDiffResult,
		type ChangesetState,
		type PreviewFileDiffs
	} from '#lib/batches/changesets.ts';
	import {
		APPLY_PREVIEW,
		BATCH_SPEC,
		OPERATIONS,
		applyMutation,
		canSetPublicationState,
		deltaSummary,
		operationLabel,
		previewSpec,
		previewTitle,
		publicationStatesInput,
		publishedLabel,
		statsOperations,
		type ApplyPreviewResult,
		type ApplyResult,
		type BatchSpecForApply,
		type BatchSpecResult,
		type ChangesetSpecOperation,
		type PreviewNode,
		type PreviewStats,
		type PublishedValue
	} from '#lib/batches/preview.ts';

	const kind = $derived((page.params.nskind ?? 'users') as NamespaceKind);
	const ns = $derived(page.params.ns ?? '');
	const specId = $derived(page.params.spec ?? '');

	const PAGE = 50;

	let spec = $state.raw<BatchSpecForApply | null>(null);
	let error = $state<string | null>(null);

	let search = $state('');
	let searchInput = $state('');
	let currentState = $state<ChangesetState | ''>('');
	let action = $state<ChangesetSpecOperation | ''>('');
	/** Publication states chosen in the UI, by changeset spec ID. */
	const choices = new SvelteMap<string, PublishedValue>();
	const choicesInput = $derived(publicationStatesInput(choices));

	let nodes = $state.raw<PreviewNode[]>([]);
	let stats = $state.raw<PreviewStats | null>(null);
	let totalCount = $state(0);
	let cursor = $state<string | null>(null);
	let hasNext = $state(false);
	let loading = $state(true);
	let previewError = $state<string | null>(null);
	let applying = $state(false);
	let applyError = $state<string | null>(null);

	const expanded = new SvelteSet<string>();
	const diffs = new SvelteMap<string, PreviewFileDiffs | string>();

	$effect(() => {
		const id = specId;
		error = null;
		graphql<BatchSpecResult, { id: string }>(BATCH_SPEC, { id })
			.then((r) => {
				if (r.node?.__typename === 'BatchSpec') spec = r.node as BatchSpecForApply;
				else error = 'Bulk change spec not found. Unapplied specs expire after a week.';
			})
			.catch((err) => (error = errorMessage(err)));
	});

	async function loadPreview(more: boolean): Promise<void> {
		loading = true;
		previewError = null;
		try {
			const r = await graphql<ApplyPreviewResult, Record<string, unknown>>(APPLY_PREVIEW, {
				batchSpec: specId,
				first: PAGE,
				after: more ? cursor : null,
				search: search.trim() || null,
				currentState: currentState || null,
				action: action || null,
				publicationStates: choicesInput.length > 0 ? choicesInput : null
			});
			const conn = r.node?.applyPreview;
			if (!conn) throw new Error('Bulk change spec not found.');
			nodes = more ? [...nodes, ...conn.nodes] : conn.nodes;
			stats = conn.stats;
			totalCount = conn.totalCount;
			cursor = conn.pageInfo.endCursor;
			hasNext = conn.pageInfo.hasNextPage;
		} catch (err) {
			previewError = errorMessage(err);
		} finally {
			loading = false;
		}
	}

	$effect(() => {
		void specId;
		void search;
		void currentState;
		void action;
		void choicesInput;
		void loadPreview(false);
	});

	function setChoice(specID: string, value: string): void {
		if (value === '') choices.delete(specID);
		else choices.set(specID, value === 'draft' ? 'draft' : value === 'true');
	}

	function choiceValue(specID: string): string {
		const v = choices.get(specID);
		return v === undefined ? '' : String(v);
	}

	/** Sets the publication state of every entry on the page that allows one. */
	function setAll(value: string): void {
		for (const n of nodes) {
			if (n.__typename !== 'VisibleChangesetApplyPreview' || !canSetPublicationState(n.targets)) continue;
			const s = previewSpec(n.targets);
			if (s) setChoice(s.id, value);
		}
	}

	const settable = $derived(
		nodes.filter((n) => n.__typename === 'VisibleChangesetApplyPreview' && canSetPublicationState(n.targets)).length
	);

	async function toggleDiff(specID: string): Promise<void> {
		if (expanded.has(specID)) {
			expanded.delete(specID);
			return;
		}
		expanded.add(specID);
		if (diffs.has(specID)) return;
		try {
			const r = await graphql<ChangesetSpecDiffResult, { id: string; first: number }>(CHANGESET_SPEC_DIFF, {
				id: specID,
				first: 200
			});
			const desc = r.node?.description;
			const d = desc?.__typename === 'GitBranchChangesetDescription' ? desc.diff.fileDiffs : undefined;
			diffs.set(specID, d ?? 'Imported changeset: no diff.');
		} catch (err) {
			diffs.set(specID, errorMessage(err));
		}
	}

	async function apply(): Promise<void> {
		if (!spec) return;
		const target = spec.appliesToBatchChange;
		const msg = target
			? `Apply the spec to the bulk change ${target.name}? Changesets are created, updated or closed as previewed.`
			: `Create the bulk change ${spec.description.name}?`;
		if (!confirm(msg)) return;
		applying = true;
		applyError = null;
		try {
			const m = applyMutation(spec, choicesInput);
			const r = await graphql<ApplyResult, Record<string, unknown>>(m.query, m.variables);
			const bc = r[m.field];
			if (bc) await goto(bc.url);
		} catch (err) {
			applyError = errorMessage(err);
		} finally {
			applying = false;
		}
	}

	function applySearch(event: SubmitEvent): void {
		event.preventDefault();
		search = searchInput;
	}
</script>

<svelte:head><title>Preview · {spec?.description.name ?? 'Bulk change spec'} · Sourcegull</title></svelte:head>

<div class="page">
	<nav class="crumbs" aria-label="Breadcrumb">
		<a href="/bulk-changes">Bulk changes</a> /
		<a href={namespaceBatchChangesURL(kind, ns)}>{ns}</a> /
	</nav>

	{#if error}
		<div class="alert alert-danger" role="alert">{error}</div>
	{/if}

	{#if spec}
		<div class="page-header">
			<div>
				<h1>{spec.description.name}</h1>
				<p class="page-description sub">
					Preview of a spec {spec.creator ? `uploaded by ${spec.creator.username} ` : ''}
					<span title={formatDate(spec.createdAt)}>{relativeTime(spec.createdAt)}</span>
					in {namespaceName(spec.namespace)}
					{#if spec.expiresAt && !spec.appliesToBatchChange}
						· expires {relativeTime(spec.expiresAt)}
					{/if}
				</p>
			</div>
		</div>

		{#if spec.description.description}
			<div class="description"><Markdown source={spec.description.description} /></div>
		{/if}

		<div class="alert alert-info apply-box" role="note">
			<div>
				{#if spec.appliesToBatchChange}
					This spec updates the bulk change
					<a href={spec.appliesToBatchChange.url}>{spec.appliesToBatchChange.name}</a>.
				{:else}
					This spec creates a new bulk change.
				{/if}
				Changesets are not published unless the spec or your choice below says so.
			</div>
			{#if spec.viewerCanAdminister}
				<button class="btn btn-primary" type="button" disabled={applying || loading} onclick={apply}>
					{applying ? 'Applying…' : spec.appliesToBatchChange ? 'Apply' : 'Create'}
				</button>
			{/if}
		</div>
		{#if applyError}
			<div class="alert alert-danger" role="alert">{applyError}</div>
		{/if}

		{#if stats}
			<section class="stats card" aria-label="Preview stats">
				<dl class="delta">
					<div data-stat="added"><dt>Added</dt><dd>{stats.added}</dd></div>
					<div data-stat="modified"><dt>Modified</dt><dd>{stats.modified}</dd></div>
					<div data-stat="removed"><dt>Removed</dt><dd>{stats.removed}</dd></div>
				</dl>
				<ul class="ops" aria-label="Operations">
					{#each statsOperations(stats) as o (o.op)}
						<li><span class="op op-{o.op.toLowerCase()}">{operationLabel(o.op)}</span> {o.count}</li>
					{/each}
				</ul>
				{#if spec.diffStat}
					<div class="small">
						<span class="added">+{spec.diffStat.added}</span>
						<span class="deleted">−{spec.diffStat.deleted}</span>
					</div>
				{/if}
			</section>
		{/if}
	{/if}

	<form class="filters" onsubmit={applySearch} aria-label="Filter the preview">
		<input
			class="input search"
			type="search"
			placeholder="Search title or repository"
			aria-label="Search the preview"
			bind:value={searchInput}
		/>
		<select class="input" aria-label="Current state" bind:value={currentState}>
			<option value="">Any current state</option>
			{#each CHANGESET_STATES as s (s)}<option value={s}>{enumLabel(s)}</option>{/each}
		</select>
		<select class="input" aria-label="Action" bind:value={action}>
			<option value="">Any action</option>
			{#each OPERATIONS as o (o)}<option value={o}>{operationLabel(o)}</option>{/each}
		</select>
		<button class="btn" type="submit">Search</button>
	</form>

	{#if settable > 0}
		<div class="toolbar publish-all">
			<span class="muted small">
				{settable === 1 ? "1 changeset leaves" : `${settable} changesets leave`} publication to you:
			</span>
			<button class="btn btn-sm" type="button" onclick={() => setAll('true')}>Publish all</button>
			<button class="btn btn-sm" type="button" onclick={() => setAll('draft')}>Publish all as drafts</button>
			<button class="btn btn-sm" type="button" onclick={() => setAll('')}>Reset</button>
		</div>
	{/if}

	{#if previewError}
		<div class="alert alert-danger" role="alert">{previewError}</div>
	{/if}

	<ul class="preview" aria-busy={loading} aria-label="Changesets">
		{#each nodes as n, i (i)}
			{#if n.__typename === 'VisibleChangesetApplyPreview'}
				{@const t = previewTitle(n.targets)}
				{@const s = previewSpec(n.targets)}
				{@const current = n.targets.__typename === 'VisibleApplyPreviewTargetsAttach' ? null : n.targets.changeset}
				{@const changes = n.targets.__typename === 'VisibleApplyPreviewTargetsUpdate' ? deltaSummary(n.delta) : []}
				<li class="entry" data-preview={t.title}>
					<div class="row">
						{#if s}
							<button
								class="btn-link expand"
								type="button"
								aria-expanded={expanded.has(s.id)}
								aria-label={expanded.has(s.id) ? 'Hide diff' : 'Show diff'}
								onclick={() => toggleDiff(s.id)}>{expanded.has(s.id) ? '▾' : '▸'}</button
							>
						{:else}
							<span class="expand"></span>
						{/if}
						<div class="ops-cell">
							{#each n.operations as op (op)}
								<span class="op op-{op.toLowerCase()}" data-operation={op}>{operationLabel(op)}</span>
							{:else}
								<span class="op">No action</span>
							{/each}
						</div>
						<div class="info">
							<div class="title">
								{#if current?.externalURL}
									<a href={current.externalURL.url} rel="noopener noreferrer">{t.title}</a>
								{:else}
									{t.title}
								{/if}
							</div>
							<div class="meta muted small">
								<span>{t.repo}</span>
								{#if t.branch}<span class="mono">{t.branch}</span>{/if}
								{#if current}
									<span>now <StateBadge state={current.state} tone={stateTone(current.state)} /></span>
								{:else}
									<span>new changeset</span>
								{/if}
								{#if changes.length > 0}<span>changes: {changes.join(', ')}</span>{/if}
							</div>
						</div>
						<div class="publish">
							{#if s && canSetPublicationState(n.targets)}
								<select
									class="input"
									aria-label="Publication state of {t.title}"
									value={choiceValue(s.id)}
									onchange={(e) => setChoice(s.id, e.currentTarget.value)}
								>
									<option value="">Unpublished</option>
									<option value="draft">Publish as draft</option>
									<option value="true">Publish</option>
								</select>
							{:else if s?.description.__typename === 'GitBranchChangesetDescription'}
								<span class="muted small" title="Set by the spec's published field">
									{publishedLabel(s.description.published)} (spec)
								</span>
							{/if}
						</div>
						<div class="diffstat small">
							{#if s?.description.__typename === 'GitBranchChangesetDescription'}
								<span class="added">+{s.description.diffStat.added}</span>
								<span class="deleted">−{s.description.diffStat.deleted}</span>
							{/if}
						</div>
					</div>
					{#if s && expanded.has(s.id)}
						{@const d = diffs.get(s.id)}
						<div class="details">
							{#if s.description.__typename === 'GitBranchChangesetDescription' && s.description.body}
								<div class="body card"><Markdown source={s.description.body} /></div>
							{/if}
							{#if d === undefined}
								<p class="muted">Loading the diff…</p>
							{:else if typeof d === 'string'}
								<p class="muted">{d}</p>
							{:else}
								<DiffView repo={t.repo} diffs={toFileDiffConnection(d)} />
							{/if}
						</div>
					{/if}
				</li>
			{:else}
				<li class="entry">
					<div class="row">
						<span class="expand"></span>
						<div class="ops-cell">
							{#each n.operations as op (op)}<span class="op op-{op.toLowerCase()}">{operationLabel(op)}</span>{/each}
						</div>
						<div class="info muted">A changeset in a repository you don't have access to.</div>
					</div>
				</li>
			{/if}
		{:else}
			{#if !loading}
				<li class="empty muted">No changesets match.</li>
			{/if}
		{/each}
	</ul>
	<div class="footer">
		<span class="muted small">{totalCount} {totalCount === 1 ? 'changeset' : 'changesets'}</span>
		{#if hasNext}
			<button class="btn btn-sm" type="button" disabled={loading} onclick={() => loadPreview(true)}>Show more</button>
		{/if}
	</div>
</div>

<style>
	.page {
		max-width: 72rem;
	}
	h1 {
		overflow-wrap: anywhere;
	}
	.sub {
		margin: var(--space-1) 0 0;
	}
	.description {
		margin-bottom: var(--space-4);
		max-width: 52rem;
	}
	.apply-box {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--space-4);
		flex-wrap: wrap;
	}
	.stats {
		display: flex;
		align-items: center;
		gap: var(--space-6);
		flex-wrap: wrap;
		padding: var(--space-4);
		margin-bottom: var(--space-4);
	}
	.delta {
		display: flex;
		gap: var(--space-4);
		margin: 0;
	}
	.delta div {
		text-align: center;
	}
	.delta dt {
		font-size: var(--text-xs);
		color: var(--color-text-muted);
	}
	.delta dd {
		margin: 0;
		font-size: var(--text-lg);
		font-weight: 600;
		font-variant-numeric: tabular-nums;
	}
	.ops {
		display: flex;
		gap: var(--space-3);
		flex-wrap: wrap;
		list-style: none;
		margin: 0;
		padding: 0;
		flex: 1;
	}
	.op {
		display: inline-block;
		padding: 0.05rem 0.45rem;
		border-radius: var(--radius);
		font-size: var(--text-xs);
		font-weight: 600;
		background: var(--color-bg-muted);
		color: var(--color-text-muted);
		white-space: nowrap;
	}
	.op-publish,
	.op-publish_draft,
	.op-import,
	.op-reattach,
	.op-reopen,
	.op-undraft {
		background: var(--color-success-bg);
		color: var(--color-success);
	}
	.op-push,
	.op-update {
		background: var(--color-info-bg);
		color: var(--color-primary);
	}
	.op-close,
	.op-archive,
	.op-detach {
		background: var(--color-danger-bg);
		color: var(--color-danger);
	}
	.filters {
		display: flex;
		gap: var(--space-2);
		flex-wrap: wrap;
		margin-bottom: var(--space-3);
	}
	.filters .input {
		width: auto;
		min-width: 0;
	}
	.search {
		flex: 1 1 14rem;
	}
	.publish-all {
		margin-bottom: var(--space-2);
	}
	.preview {
		list-style: none;
		margin: 0;
		padding: 0;
		background: var(--color-surface);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-lg);
		box-shadow: var(--shadow-xs);
		overflow: hidden;
	}
	.entry {
		border-bottom: 1px solid var(--color-border);
	}
	.entry:last-child {
		border-bottom: 0;
	}
	.row {
		display: flex;
		align-items: flex-start;
		gap: var(--space-3);
		padding: var(--space-2) var(--space-3);
	}
	.expand {
		width: 1rem;
		flex: none;
		padding: 0;
	}
	.ops-cell {
		width: 9rem;
		flex: none;
		display: flex;
		flex-wrap: wrap;
		gap: var(--space-1);
	}
	.info {
		flex: 1;
		min-width: 0;
	}
	.title {
		font-weight: 500;
		overflow-wrap: anywhere;
	}
	.meta {
		display: flex;
		gap: var(--space-3);
		flex-wrap: wrap;
		overflow-wrap: anywhere;
	}
	.publish {
		width: 11rem;
		flex: none;
	}
	.publish select {
		width: 100%;
	}
	.diffstat {
		width: 5rem;
		flex: none;
		text-align: right;
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
	.details {
		padding: 0 var(--space-3) var(--space-3) var(--space-8);
		min-width: 0;
	}
	.body {
		padding: var(--space-3);
		margin-bottom: var(--space-2);
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
	@media (max-width: 48rem) {
		.row {
			flex-wrap: wrap;
		}
		.ops-cell {
			width: auto;
		}
		.info {
			flex-basis: 100%;
			order: 5;
		}
		.publish {
			order: 6;
		}
		.diffstat {
			order: 7;
			width: auto;
		}
		.details {
			padding-left: 0;
		}
	}
</style>
