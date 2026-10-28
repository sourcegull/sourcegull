<!--
	The changesets of a bulk change (the changesets tab): filters (state,
	review state, check state, search), rows with state, title, repository, branch, diff stat,
	review and check state, an expandable body and diff from the spec; selection with the
	operations every selected changeset allows, bulk actions and the CSV export. With `archived`,
	only archived changesets (the Archived tab).
-->
<script lang="ts">
	import { onDestroy, untrack } from 'svelte';
	import { SvelteMap, SvelteSet } from 'svelte/reactivity';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import DiffView from '#lib/repo/DiffView.svelte';
	import { formatDate, relativeTime, type BulkOperationType } from './batch-change.ts';
	import {
		AVAILABLE_BULK_OPERATIONS,
		BULK_ACTIONS,
		CHANGESETS,
		CHANGESETS_CSV,
		CHANGESET_IDS,
		CHANGESET_SPEC_DIFF,
		CHANGESET_STATES,
		CHECK_STATES,
		REVIEW_STATES,
		bulkMutation,
		changesetBranch,
		csvFileName,
		enumLabel,
		filterVariables,
		stateTone,
		toFileDiffConnection,
		type AvailableBulkOperationsResult,
		type ChangesetFilters,
		type ChangesetIDsResult,
		type ChangesetNode,
		type ChangesetsCSVResult,
		type ChangesetSpecDiffResult,
		type ChangesetsResult,
		type ExternalChangesetNode,
		type PreviewFileDiffs
	} from './changesets.ts';
	import Markdown from './Markdown.svelte';
	import StateBadge from './StateBadge.svelte';

	interface Props {
		batchChange: { id: string; name: string; viewerCanAdminister: boolean };
		/** Only archived changesets. */
		archived?: boolean;
		/** Called after a bulk operation was queued. */
		onbulk?: () => void;
	}

	let { batchChange, archived = false, onbulk }: Props = $props();

	const PAGE = 50;

	let filters = $state<ChangesetFilters>({
		state: '',
		reviewState: '',
		checkState: '',
		search: '',
		onlyArchived: false
	});
	let searchInput = $state('');

	let nodes = $state.raw<ChangesetNode[]>([]);
	let totalCount = $state(0);
	let cursor = $state<string | null>(null);
	let hasNext = $state(false);
	let loading = $state(true);
	let error = $state<string | null>(null);

	const selected = new SvelteSet<string>();
	/** Every changeset matching the filters is selected (IDs fetched at once). */
	let allSelected = $state(false);
	let available = $state.raw<BulkOperationType[]>([]);
	let action = $state<BulkOperationType | ''>('');
	let commentBody = $state('');
	let squash = $state(false);
	let draft = $state(false);
	let submitting = $state(false);
	let notice = $state<string | null>(null);

	const expanded = new SvelteSet<string>();
	const diffs = new SvelteMap<string, PreviewFileDiffs | string>();

	const vars = $derived({ ...filterVariables(filters), onlyArchived: archived });

	async function load(more = false): Promise<void> {
		loading = true;
		error = null;
		try {
			const r = await graphql<ChangesetsResult, Record<string, unknown>>(CHANGESETS, {
				batchChange: batchChange.id,
				first: PAGE,
				after: more ? cursor : null,
				...vars
			});
			const conn = r.node?.changesets;
			if (!conn) throw new Error('Bulk change not found.');
			nodes = more ? [...nodes, ...conn.nodes] : conn.nodes;
			totalCount = conn.totalCount;
			cursor = conn.pageInfo.endCursor;
			hasNext = conn.pageInfo.hasNextPage;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loading = false;
		}
	}

	$effect(() => {
		void vars;
		untrack(() => {
			selected.clear();
			allSelected = false;
			void load();
		});
	});

	// Changesets the reconciler is working on change state by themselves: refresh while any is.
	let poll: ReturnType<typeof setTimeout> | undefined;
	$effect(() => {
		clearTimeout(poll);
		if (nodes.some((n) => n.state === 'PROCESSING' || n.state === 'RETRYING')) {
			poll = setTimeout(() => void refresh(), 5000);
		}
	});
	onDestroy(() => clearTimeout(poll));

	/** Reloads the pages loaded so far without resetting the selection. */
	async function refresh(): Promise<void> {
		try {
			const r = await graphql<ChangesetsResult, Record<string, unknown>>(CHANGESETS, {
				batchChange: batchChange.id,
				first: Math.max(PAGE, nodes.length),
				after: null,
				...vars
			});
			const conn = r.node?.changesets;
			if (!conn) return;
			nodes = conn.nodes;
			totalCount = conn.totalCount;
			cursor = conn.pageInfo.endCursor;
			hasNext = conn.pageInfo.hasNextPage;
		} catch {
			// The next action reports errors.
		}
	}

	// The operations every selected changeset allows.
	$effect(() => {
		const ids = [...selected];
		if (ids.length === 0) {
			available = [];
			return;
		}
		let stale = false;
		graphql<AvailableBulkOperationsResult, { batchChange: string; changesets: string[] }>(
			AVAILABLE_BULK_OPERATIONS,
			{ batchChange: batchChange.id, changesets: ids }
		)
			.then((r) => {
				if (stale) return;
				available = r.node?.availableBulkOperations ?? [];
				if (action && !available.includes(action)) action = '';
			})
			.catch((err) => {
				if (!stale) error = errorMessage(err);
			});
		return () => {
			stale = true;
		};
	});

	const visible = $derived(nodes.filter((n): n is ExternalChangesetNode => n.__typename === 'ExternalChangeset'));
	const pageSelected = $derived(visible.length > 0 && visible.every((n) => selected.has(n.id)));

	function togglePage(): void {
		allSelected = false;
		if (pageSelected) for (const n of visible) selected.delete(n.id);
		else for (const n of visible) selected.add(n.id);
	}

	function toggle(id: string): void {
		allSelected = false;
		if (selected.has(id)) selected.delete(id);
		else selected.add(id);
	}

	async function selectAll(): Promise<void> {
		try {
			const r = await graphql<ChangesetIDsResult, Record<string, unknown>>(CHANGESET_IDS, {
				batchChange: batchChange.id,
				first: 10000,
				...vars
			});
			for (const n of r.node?.changesets?.nodes ?? []) {
				if (n.__typename === 'ExternalChangeset') selected.add(n.id);
			}
			allSelected = true;
		} catch (err) {
			error = errorMessage(err);
		}
	}

	function clearSelection(): void {
		selected.clear();
		allSelected = false;
		action = '';
	}

	const actionInfo = $derived(BULK_ACTIONS.find((a) => a.type === action));

	async function runBulk(event: SubmitEvent): Promise<void> {
		event.preventDefault();
		if (!action) return;
		const ids = [...selected];
		const info = BULK_ACTIONS.find((a) => a.type === action);
		if (!confirm(`${info?.title ?? action}: ${ids.length} ${ids.length === 1 ? 'changeset' : 'changesets'}?`)) return;
		submitting = true;
		error = null;
		notice = null;
		try {
			const m = bulkMutation(action, batchChange.id, ids, { body: commentBody, squash, draft });
			await graphql(m.query, m.variables);
			notice = `${info?.title ?? 'Bulk operation'} started for ${ids.length} ${ids.length === 1 ? 'changeset' : 'changesets'}. Its progress is on the Bulk operations tab.`;
			clearSelection();
			commentBody = '';
			onbulk?.();
			await refresh();
		} catch (err) {
			error = errorMessage(err);
		} finally {
			submitting = false;
		}
	}

	async function exportCSV(): Promise<void> {
		error = null;
		try {
			const r = await graphql<ChangesetsCSVResult, Record<string, unknown>>(CHANGESETS_CSV, {
				batchChange: batchChange.id,
				changesets: selected.size > 0 ? [...selected] : null,
				...vars
			});
			const csv = r.node?.changesetsCSV ?? '';
			const a = document.createElement('a');
			a.href = URL.createObjectURL(new Blob([csv], { type: 'text/csv' }));
			a.download = csvFileName(batchChange.name);
			a.click();
			setTimeout(() => URL.revokeObjectURL(a.href), 1000);
		} catch (err) {
			error = errorMessage(err);
		}
	}

	async function toggleExpand(c: ExternalChangesetNode): Promise<void> {
		if (expanded.has(c.id)) {
			expanded.delete(c.id);
			return;
		}
		expanded.add(c.id);
		const spec = c.currentSpec?.id;
		if (!spec || diffs.has(spec)) return;
		try {
			const r = await graphql<ChangesetSpecDiffResult, { id: string; first: number }>(CHANGESET_SPEC_DIFF, {
				id: spec,
				first: 200
			});
			const desc = r.node?.description;
			const d = desc?.__typename === 'GitBranchChangesetDescription' ? desc.diff.fileDiffs : undefined;
			diffs.set(spec, d ?? 'No diff: the changeset was imported.');
		} catch (err) {
			diffs.set(spec, errorMessage(err));
		}
	}

	function applySearch(event: SubmitEvent): void {
		event.preventDefault();
		filters.search = searchInput;
	}
</script>

<form class="filters" onsubmit={applySearch} aria-label="Filter changesets">
	<input
		class="input search"
		type="search"
		placeholder="Search title or repository"
		aria-label="Search changesets"
		bind:value={searchInput}
	/>
	{#if !archived}
		<select class="input" aria-label="State" bind:value={filters.state}>
			<option value="">Any state</option>
			{#each CHANGESET_STATES as s (s)}<option value={s}>{enumLabel(s)}</option>{/each}
		</select>
	{/if}
	<select class="input" aria-label="Review state" bind:value={filters.reviewState}>
		<option value="">Any review state</option>
		{#each REVIEW_STATES as s (s)}<option value={s}>{enumLabel(s)}</option>{/each}
	</select>
	<select class="input" aria-label="Check state" bind:value={filters.checkState}>
		<option value="">Any check state</option>
		{#each CHECK_STATES as s (s)}<option value={s}>{enumLabel(s)}</option>{/each}
	</select>
	<button class="btn" type="submit">Search</button>
</form>

{#if error}
	<div class="alert alert-danger" role="alert">{error}</div>
{/if}
{#if notice}
	<div class="alert alert-success" role="status">{notice}</div>
{/if}

<div class="toolbar list-toolbar">
	{#if batchChange.viewerCanAdminister}
		<label class="checkbox">
			<input type="checkbox" checked={pageSelected} onchange={togglePage} aria-label="Select all visible changesets" />
			{selected.size > 0 ? `${selected.size} selected` : 'Select'}
		</label>
		{#if selected.size > 0 && !allSelected && totalCount > selected.size}
			<button class="btn-link" type="button" onclick={selectAll}>Select all {totalCount}</button>
		{/if}
		{#if selected.size > 0}
			<button class="btn-link" type="button" onclick={clearSelection}>Clear</button>
		{/if}
	{/if}
	<span class="spacer"></span>
	<button class="btn btn-sm" type="button" onclick={exportCSV}>
		Export CSV{selected.size > 0 ? ` (${selected.size})` : ''}
	</button>
</div>

{#if selected.size > 0}
	<form class="bulk card" onsubmit={runBulk} aria-label="Bulk action">
		<div class="toolbar">
			<label for="bulk-action">Action</label>
			<select id="bulk-action" class="input" bind:value={action}>
				<option value="">Choose an action…</option>
				{#each BULK_ACTIONS as a (a.type)}
					<option value={a.type} disabled={!available.includes(a.type)}>{a.title}</option>
				{/each}
			</select>
			{#if available.length === 0}
				<span class="muted small">No action applies to every selected changeset.</span>
			{/if}
		</div>
		{#if actionInfo}
			<p class="muted small">{actionInfo.description}</p>
			{#if actionInfo.input === 'body'}
				<label for="bulk-comment">Comment</label>
				<textarea id="bulk-comment" class="input" rows="3" bind:value={commentBody} required></textarea>
			{:else if actionInfo.input === 'squash'}
				<label class="checkbox"><input type="checkbox" bind:checked={squash} /> Squash commits</label>
			{:else if actionInfo.input === 'draft'}
				<label class="checkbox"><input type="checkbox" bind:checked={draft} /> Publish as draft</label>
			{/if}
			<div class="toolbar">
				<button
					class="btn btn-primary"
					type="submit"
					disabled={submitting || (actionInfo.input === 'body' && !commentBody.trim())}
				>
					{submitting ? 'Starting…' : `${actionInfo.title} ${selected.size} ${selected.size === 1 ? 'changeset' : 'changesets'}`}
				</button>
			</div>
		{/if}
	</form>
{/if}

<ul class="changesets" aria-busy={loading} aria-label="Changesets">
	{#each nodes as c (c.id)}
		{#if c.__typename === 'ExternalChangeset'}
			{@const branch = changesetBranch(c)}
			<li class="changeset" data-changeset={c.title ?? c.id}>
				<div class="row">
					{#if batchChange.viewerCanAdminister}
						<input
							type="checkbox"
							checked={selected.has(c.id)}
							onchange={() => toggle(c.id)}
							aria-label="Select {c.title ?? 'changeset'}"
						/>
					{/if}
					<button
						class="btn-link expand"
						type="button"
						aria-expanded={expanded.has(c.id)}
						aria-label={expanded.has(c.id) ? 'Hide details' : 'Show details'}
						onclick={() => toggleExpand(c)}>{expanded.has(c.id) ? '▾' : '▸'}</button
					>
					<div class="state"><StateBadge state={c.state} tone={stateTone(c.state)} /></div>
					<div class="info">
						<div class="title">
							{#if c.externalURL}
								<a href={c.externalURL.url} rel="noopener noreferrer">{c.title ?? 'Untitled'}</a>
								<span class="muted">!{c.externalID}</span>
							{:else}
								<span>{c.title ?? 'Untitled'}</span>
							{/if}
							{#each c.labels as l (l.text)}
								<span class="badge label" style:border-color={l.color ? `#${l.color}` : undefined}>{l.text}</span>
							{/each}
						</div>
						<div class="meta muted small">
							<a class="repo" href={c.repository.url}>{c.repository.name}</a>
							{#if branch}
								<span class="mono branch">{c.forkNamespace ? `${c.forkNamespace}:` : ''}{branch}</span>
							{/if}
							{#if c.state === 'SCHEDULED'}
								<span class="schedule">
									{c.scheduleEstimateAt
										? `Publishes ${relativeTime(c.scheduleEstimateAt)} (${formatDate(c.scheduleEstimateAt)})`
										: 'Waiting for a rollout window'}
								</span>
							{/if}
						</div>
						{#if c.error}
							<div class="error small" role="note">{c.error}</div>
						{/if}
						{#if c.syncerError}
							<div class="error small" role="note">Sync: {c.syncerError}</div>
						{/if}
					</div>
					<div class="review">
						{#if c.checkState}
							<span class="small check check-{c.checkState.toLowerCase()}" title="Checks">
								Checks: {enumLabel(c.checkState)}
							</span>
						{/if}
						{#if c.reviewState}
							<span class="small" title="Review">Review: {enumLabel(c.reviewState)}</span>
						{/if}
					</div>
					<div class="diffstat small" aria-label="Diff stat">
						{#if c.diffStat}
							<span class="added">+{c.diffStat.added}</span>
							<span class="deleted">−{c.diffStat.deleted}</span>
						{/if}
					</div>
				</div>
				{#if expanded.has(c.id)}
					<div class="details">
						{#if c.body}
							<div class="body card"><Markdown source={c.body} /></div>
						{/if}
						{#if c.currentSpec}
							{@const d = diffs.get(c.currentSpec.id)}
							{#if d === undefined}
								<p class="muted">Loading the diff…</p>
							{:else if typeof d === 'string'}
								<p class="muted">{d}</p>
							{:else}
								<DiffView repo={c.repository.name} diffs={toFileDiffConnection(d)} />
							{/if}
						{:else}
							<p class="muted">Imported changeset: no spec diff.</p>
						{/if}
					</div>
				{/if}
			</li>
		{:else}
			<li class="changeset hidden-changeset">
				<div class="row">
					<div class="state"><StateBadge state={c.state} tone={stateTone(c.state)} /></div>
					<div class="info muted">A changeset in a repository you don't have access to.</div>
				</div>
			</li>
		{/if}
	{:else}
		{#if !loading}
			<li class="empty muted">{archived ? 'No archived changesets.' : 'No changesets match.'}</li>
		{/if}
	{/each}
</ul>

<div class="footer">
	<span class="muted small">{totalCount} {totalCount === 1 ? 'changeset' : 'changesets'}</span>
	{#if hasNext}
		<button class="btn btn-sm" type="button" disabled={loading} onclick={() => load(true)}>Show more</button>
	{/if}
</div>

<style>
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
	.list-toolbar {
		padding: var(--space-2) 0;
		border-bottom: 1px solid var(--color-border);
	}
	.spacer {
		flex: 1;
	}
	.bulk {
		margin: var(--space-3) 0;
		padding: var(--space-4);
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
	}
	.bulk p {
		margin: 0;
	}
	.bulk select {
		width: auto;
	}
	.changesets {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.changeset {
		border-bottom: 1px solid var(--color-border);
	}
	.row {
		display: flex;
		align-items: flex-start;
		gap: var(--space-3);
		padding: var(--space-2) 0;
	}
	.row > input {
		margin-top: 0.3rem;
	}
	.expand {
		width: 1rem;
		padding: 0;
	}
	.state {
		width: 7rem;
		flex: none;
	}
	.info {
		flex: 1;
		min-width: 0;
	}
	.title {
		display: flex;
		gap: var(--space-2);
		flex-wrap: wrap;
		align-items: baseline;
		font-weight: 500;
		overflow-wrap: anywhere;
	}
	.label {
		border: 1px solid var(--color-border);
		font-weight: 500;
	}
	.meta {
		display: flex;
		gap: var(--space-3);
		flex-wrap: wrap;
	}
	.repo {
		color: var(--color-text-muted);
		overflow-wrap: anywhere;
	}
	.branch {
		overflow-wrap: anywhere;
	}
	.schedule {
		color: var(--color-primary);
	}
	.error {
		color: var(--color-danger);
		overflow-wrap: anywhere;
		white-space: pre-wrap;
	}
	.review {
		display: flex;
		flex-direction: column;
		width: 11rem;
		flex: none;
		color: var(--color-text-muted);
	}
	.check-passed {
		color: var(--color-success);
	}
	.check-failed {
		color: var(--color-danger);
	}
	.diffstat {
		width: 6rem;
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
		padding: 0 0 var(--space-3) var(--space-8);
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
	.small {
		font-size: 0.875em;
	}
	@media (max-width: 48rem) {
		.row {
			flex-wrap: wrap;
		}
		.state {
			width: auto;
		}
		.info {
			flex-basis: 100%;
			order: 5;
		}
		.review {
			width: auto;
			flex-direction: row;
			gap: var(--space-2);
			order: 6;
		}
		.diffstat {
			width: auto;
			order: 7;
		}
		.details {
			padding-left: 0;
		}
	}
</style>
