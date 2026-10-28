<!--
	The workspace preview of a spec in the editor: the resolution's
	state, the resolved workspaces (repository, branch, path, cached results) with the reasons
	skipped ones do not run, and the `allowIgnored`/`allowUnsupported` switches.
-->
<script lang="ts">
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		WORKSPACES_PREVIEW,
		isResolving,
		resolutionLabel,
		skipReasons,
		type EditorSpec,
		type PreviewWorkspace,
		type SpecFlags,
		type WorkspacesPreviewResult
	} from './spec.ts';

	interface Props {
		/** The spec whose workspaces are shown; null before the first valid spec. */
		spec: EditorSpec | null;
		/** The spec text differs from the spec's (a validation is pending). */
		stale?: boolean;
		/** The switches can be changed (the viewer may edit the spec). */
		editable?: boolean;
		/** Saves the spec again with other flags. */
		onflags?: (flags: Pick<SpecFlags, 'allowIgnored' | 'allowUnsupported'>) => void;
	}

	let { spec, stale = false, editable = false, onflags }: Props = $props();

	const PAGE = 50;

	let workspaces = $state.raw<PreviewWorkspace[]>([]);
	let totalCount = $state(0);
	let cursor = $state<string | null>(null);
	let hasMore = $state(false);
	let search = $state('');
	let loading = $state(false);
	let error = $state<string | null>(null);
	let seq = 0;

	const resolution = $derived(spec?.workspaceResolution ?? null);
	const flags = $derived({
		allowIgnored: spec?.allowIgnored === true,
		allowUnsupported: spec?.allowUnsupported === true
	});
	/** Reload when the spec or its resolution changes. */
	const version = $derived(
		spec ? `${spec.id}:${spec.originalInput.length}:${resolution?.state ?? ''}:${resolution?.finishedAt ?? ''}` : ''
	);

	async function load(after: string | null): Promise<void> {
		if (!spec || resolution?.state !== 'COMPLETED') {
			workspaces = [];
			totalCount = 0;
			hasMore = false;
			return;
		}
		const mine = ++seq;
		loading = true;
		error = null;
		try {
			const r = await graphql<
				WorkspacesPreviewResult,
				{ id: string; first: number; after: string | null; search: string | null }
			>(WORKSPACES_PREVIEW, { id: spec.id, first: PAGE, after, search: search.trim() || null });
			if (mine !== seq) return;
			const node = r.node;
			const page = node?.__typename === 'BatchSpec' ? node.workspaceResolution?.workspaces : undefined;
			if (!page) return;
			workspaces = after ? [...workspaces, ...page.nodes] : page.nodes;
			totalCount = page.totalCount;
			cursor = page.pageInfo.endCursor;
			hasMore = page.pageInfo.hasNextPage;
		} catch (err) {
			if (mine === seq) error = errorMessage(err);
		} finally {
			if (mine === seq) loading = false;
		}
	}

	$effect(() => {
		void version;
		void load(null);
	});

	let debounce: ReturnType<typeof setTimeout> | undefined;
	function onsearch(): void {
		clearTimeout(debounce);
		debounce = setTimeout(() => void load(null), 250);
	}
</script>

<section class="preview" aria-labelledby="ws-preview-title" aria-busy={loading || isResolving(resolution?.state)}>
	<div class="head">
		<h2 id="ws-preview-title">Workspaces preview</h2>
		{#if resolution?.state === 'COMPLETED'}
			<span class="badge" data-testid="workspace-count">{resolution.workspaces.totalCount}</span>
		{/if}
	</div>

	<p class="status muted" role="status">
		{#if stale}
			Validating the spec…
		{:else}
			{resolutionLabel(resolution?.state)}
		{/if}
	</p>
	{#if resolution?.failureMessage}
		<div class="alert alert-danger" role="alert">{resolution.failureMessage}</div>
	{/if}
	{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}

	<fieldset class="flags" disabled={!editable || !spec}>
		<legend class="visually-hidden">Workspaces to include</legend>
		<label class="checkbox">
			<input
				type="checkbox"
				checked={flags.allowIgnored}
				onchange={(e) => onflags?.({ ...flags, allowIgnored: e.currentTarget.checked })}
			/>
			<span>Include repositories with a <code>.batchignore</code> file</span>
		</label>
		<label class="checkbox">
			<input
				type="checkbox"
				checked={flags.allowUnsupported}
				onchange={(e) => onflags?.({ ...flags, allowUnsupported: e.currentTarget.checked })}
			/>
			Include repositories on unsupported code hosts
		</label>
	</fieldset>

	{#if resolution?.state === 'COMPLETED'}
		<input
			class="input search"
			type="search"
			placeholder="Filter by repository…"
			aria-label="Filter workspaces"
			bind:value={search}
			oninput={onsearch}
		/>
		<ul class="workspaces" aria-label="Workspaces">
			{#each workspaces as ws (ws.id)}
				{@const reasons = skipReasons(ws, flags)}
				<li class="ws" class:skipped={reasons.length > 0} data-repo={ws.repository?.name ?? ''}>
					<div class="line">
						{#if ws.repository}
							<a class="repo mono" href={ws.repository.url}>{ws.repository.name}</a>
						{:else}
							<span class="muted">Hidden repository</span>
						{/if}
						{#if ws.cachedResultFound}
							<span class="badge ok" title="The whole result comes from the cache">cached</span>
						{:else if ws.stepCacheResultCount > 0}
							<span class="badge ok" title="Steps whose result comes from the cache"
								>{ws.stepCacheResultCount} cached {ws.stepCacheResultCount === 1 ? 'step' : 'steps'}</span
							>
						{/if}
						{#if ws.ignored}<span class="badge warn">ignored</span>{/if}
						{#if ws.unsupported}<span class="badge warn">unsupported</span>{/if}
					</div>
					{#if ws.branch || ws.path}
						<div class="meta muted small">
							{#if ws.branch}<span class="mono">{ws.branch.displayName}</span>{/if}
							{#if ws.path}<span> · <span class="mono">{ws.path}</span></span>{/if}
							{#if ws.onlyFetchWorkspace}<span> · only the workspace is fetched</span>{/if}
							{#if ws.searchResultPaths && ws.searchResultPaths.length > 0}
								<span title={ws.searchResultPaths.join('\n')}>
									· {ws.searchResultPaths.length} matching {ws.searchResultPaths.length === 1 ? 'file' : 'files'}</span
								>
							{/if}
						</div>
					{/if}
					{#each reasons as reason (reason)}
						<div class="reason small">Skipped: {reason}</div>
					{/each}
				</li>
			{:else}
				<li class="muted empty">
					{loading ? 'Loading…' : search ? 'No workspaces match.' : 'The spec matches no repositories.'}
				</li>
			{/each}
		</ul>
		{#if hasMore}
			<button class="btn btn-sm" type="button" disabled={loading} onclick={() => load(cursor)}>
				Show more ({workspaces.length} of {totalCount})
			</button>
		{/if}
	{/if}
</section>

<style>
	.preview {
		min-width: 0;
	}
	.head {
		display: flex;
		align-items: center;
		gap: var(--space-2);
	}
	.head h2 {
		margin: 0;
	}
	.status {
		margin: var(--space-2) 0;
	}
	.flags {
		border: 0;
		margin: 0 0 var(--space-3);
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: var(--space-1);
	}
	.search {
		margin-bottom: var(--space-2);
	}
	.workspaces {
		list-style: none;
		margin: 0 0 var(--space-2);
		padding: 0;
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
		max-height: 60vh;
		overflow-y: auto;
	}
	.ws,
	.empty {
		padding: var(--space-2) var(--space-3);
		border-bottom: 1px solid var(--color-border);
	}
	.ws:last-child {
		border-bottom: 0;
	}
	.ws.skipped .repo {
		color: var(--color-text-muted);
	}
	.line {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: var(--space-2);
	}
	.repo {
		overflow-wrap: anywhere;
		font-weight: 600;
	}
	.small {
		font-size: 0.875em;
	}
	.meta {
		overflow-wrap: anywhere;
	}
	.reason {
		color: var(--color-warning);
	}
	.badge.ok {
		background: var(--color-success-bg);
		color: var(--color-success);
	}
	.badge.warn {
		background: var(--color-warning-bg);
		color: var(--color-warning);
	}
</style>
