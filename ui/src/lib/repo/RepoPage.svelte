<script lang="ts">
	import { afterNavigate, goto } from '$app/navigation';
	import { onMount } from 'svelte';
	import Icon from '#lib/components/Icon.svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import type { ViewerPermissionsQuery } from '#lib/graphql-types.ts';
	import { readStorage, writeStorage } from '#lib/theme/theme.ts';
	import CodeGraphView from './CodeGraphView.svelte';
	import CodeHostLink from './CodeHostLink.svelte';
	import type { CodeHostTarget } from './codehost.ts';
	import CommitList from './CommitList.svelte';
	import CommitView from './CommitView.svelte';
	import CompareView from './CompareView.svelte';
	import FileTree from './FileTree.svelte';
	import FileView from './FileView.svelte';
	import { emptyRepoMessage, loadRepositoryHeader, type RepositoryHeader } from './queries.ts';
	import { canWriteMetadata, VIEWER_PERMISSIONS } from './metadata.ts';
	import RefsView from './RefsView.svelte';
	import RepoSettingsView from './RepoSettingsView.svelte';
	import RevisionSwitcher from './RevisionSwitcher.svelte';
	import TreeView from './TreeView.svelte';
	import { repoPageURL, repoURL, withRevision, type RepoRoute } from './url.ts';

	interface Props {
		route: RepoRoute;
	}

	let { route }: Props = $props();

	let header = $state.raw<RepositoryHeader | null>(null);
	let notFound = $state(false);
	let error = $state<string | null>(null);
	let loadedRepo = '';

	$effect(() => {
		const name = route.repo;
		if (name === loadedRepo) return;
		loadedRepo = name;
		header = null;
		notFound = false;
		error = null;
		loadRepositoryHeader(name)
			.then((h) => {
				if (loadedRepo !== name) return;
				header = h;
				notFound = h === null;
			})
			.catch((err: unknown) => {
				if (loadedRepo === name) error = errorMessage(err);
			});
	});

	/** Whether the viewer may change repository metadata (the Settings tab). */
	let canSettings = $state(false);
	onMount(() => {
		graphql<ViewerPermissionsQuery>(VIEWER_PERMISSIONS)
			.then((r) => (canSettings = canWriteMetadata(r.currentUser)))
			.catch(() => {});
	});

	const browsing = $derived(route.kind === 'root' || route.kind === 'tree' || route.kind === 'blob');
	const showSwitcher = $derived(browsing || route.kind === 'commits');
	const defaultBranch = $derived(header?.defaultBranch?.displayName ?? null);
	// The repo node is the source of truth: `defaultBranch` is null only when it cannot list refs
	// (not cloned yet); `mirrorInfo` may lag behind it.
	const cloned = $derived(header === null || header.mirrorInfo.cloned || defaultBranch !== null);
	// Cloned without commits (no repository on GitLab yet, or no branch pushed): a notice, not
	// a browse view that would fail on the missing HEAD.
	const emptyMessage = $derived(
		header && defaultBranch === null && route.rev === null ? emptyRepoMessage(header.mirrorInfo) : null
	);

	/** "View on GitLab" follows the page: the file or directory at the revision, or the commit. */
	const codeHostTarget: CodeHostTarget = $derived.by(() => {
		const rev = route.rev ?? defaultBranch ?? 'HEAD';
		if (route.kind === 'blob' || (route.kind === 'tree' && route.path)) {
			return { kind: route.kind, rev, path: route.path };
		}
		if (route.kind === 'commit' && route.path) return { kind: 'commit', oid: route.path };
		if (route.kind === 'root' && route.rev) return { kind: 'tree', rev: route.rev, path: '' };
		return { kind: 'repo' };
	});

	const tabs = $derived([
		{ label: 'Code', href: repoURL(route.repo, route.rev), active: browsing },
		{ label: 'Commits', href: repoPageURL(route.repo, route.rev, 'commits'), active: route.kind === 'commits' || route.kind === 'commit' },
		{ label: 'Branches', href: repoPageURL(route.repo, null, 'branches'), active: route.kind === 'branches' },
		{ label: 'Tags', href: repoPageURL(route.repo, null, 'tags'), active: route.kind === 'tags' },
		{ label: 'Compare', href: repoPageURL(route.repo, null, 'compare'), active: route.kind === 'compare' },
		{ label: 'Code graph', href: repoPageURL(route.repo, null, 'code-graph'), active: route.kind === 'code-graph' },
		...(canSettings || route.kind === 'settings'
			? [{ label: 'Settings', href: repoPageURL(route.repo, null, 'settings'), active: route.kind === 'settings' }]
			: [])
	]);

	function onselect(rev: string | null): void {
		void goto(withRevision(route, rev));
	}

	// The file tree: hidden or resized on wide screens (kept per browser), a drawer on narrow ones.
	const TREE_HIDDEN_KEY = 'sourcegull.repo.tree.hidden';
	const TREE_WIDTH_KEY = 'sourcegull.repo.tree.width';
	const TREE_MIN = 180;
	const TREE_MAX = 560;
	const NARROW_QUERY = '(max-width: 760px)';

	function clampWidth(w: number): number {
		return Math.round(Math.min(TREE_MAX, Math.max(TREE_MIN, w)));
	}

	let treeHidden = $state(readStorage(TREE_HIDDEN_KEY) === 'true');
	let treeWidth = $state(clampWidth(Number(readStorage(TREE_WIDTH_KEY)) || 288));
	let narrow = $state(false);
	let treeDrawer = $state(false);
	const treeShown = $derived(narrow ? treeDrawer : !treeHidden);

	onMount(() => {
		const mq = window.matchMedia(NARROW_QUERY);
		const update = () => {
			narrow = mq.matches;
			if (!narrow) treeDrawer = false;
		};
		update();
		mq.addEventListener('change', update);
		return () => mq.removeEventListener('change', update);
	});

	afterNavigate(() => {
		treeDrawer = false;
	});

	function toggleTree(): void {
		if (narrow) {
			treeDrawer = !treeDrawer;
			return;
		}
		treeHidden = !treeHidden;
		writeStorage(TREE_HIDDEN_KEY, treeHidden ? 'true' : null);
	}

	function setWidth(w: number): void {
		treeWidth = clampWidth(w);
		writeStorage(TREE_WIDTH_KEY, String(treeWidth));
	}

	function onresizestart(e: PointerEvent): void {
		const handle = e.currentTarget as HTMLElement;
		const startX = e.clientX;
		const startW = treeWidth;
		handle.setPointerCapture(e.pointerId);
		const move = (ev: PointerEvent) => {
			treeWidth = clampWidth(startW + ev.clientX - startX);
		};
		const up = () => {
			handle.removeEventListener('pointermove', move);
			handle.removeEventListener('pointerup', up);
			handle.removeEventListener('pointercancel', up);
			setWidth(treeWidth);
		};
		handle.addEventListener('pointermove', move);
		handle.addEventListener('pointerup', up);
		handle.addEventListener('pointercancel', up);
	}

	function onresizekey(e: KeyboardEvent): void {
		const step = e.shiftKey ? 64 : 16;
		if (e.key === 'ArrowLeft') setWidth(treeWidth - step);
		else if (e.key === 'ArrowRight') setWidth(treeWidth + step);
		else if (e.key === 'Home') setWidth(TREE_MIN);
		else if (e.key === 'End') setWidth(TREE_MAX);
		else return;
		e.preventDefault();
	}

	const owner = $derived(route.repo.includes('/') ? route.repo.slice(0, route.repo.lastIndexOf('/') + 1) : '');
	const shortName = $derived(route.repo.slice(owner.length));

	const title = $derived(
		route.path && browsing ? `${route.path.split('/').pop()} · ${route.repo}` : route.repo
	);
</script>

<svelte:head><title>{title} · Sourcegull</title></svelte:head>

<div class="repo-page">
	<div class="repo-header">
		<div class="repo-title-row">
			{#if browsing}
				<button
					type="button"
					class="btn btn-ghost btn-icon btn-sm tree-toggle"
					aria-label={treeShown ? 'Hide file tree' : 'Show file tree'}
					aria-controls="repo-file-tree"
					aria-expanded={treeShown}
					title={treeShown ? 'Hide file tree' : 'Show file tree'}
					onclick={toggleTree}
				>
					<Icon name="sidebar" size={16} />
				</button>
			{/if}
			<span class="repo-icon" aria-hidden="true"><Icon name="repo" size={16} /></span>
			<h1 class="repo-name">
				<a href={repoURL(route.repo, null)}><span class="repo-owner">{owner}</span><span class="repo-short">{shortName}</span></a>
			</h1>
			{#if showSwitcher && header && cloned}
				<RevisionSwitcher repo={route.repo} current={route.rev} {defaultBranch} {onselect} />
			{/if}
			<div class="spacer"></div>
			{#each header?.externalURLs ?? [] as link (link.url)}
				<CodeHostLink class="btn btn-sm" {link} target={codeHostTarget} label />
			{/each}
		</div>
		<nav class="tabs repo-tabs" aria-label="Repository">
			{#each tabs as t (t.label)}
				<a href={t.href} aria-current={t.active ? 'page' : undefined}>{t.label}</a>
			{/each}
		</nav>
	</div>

	{#if error}
		<div class="alert alert-danger banner" role="alert">{error}</div>
	{:else if notFound}
		<div class="alert alert-warning banner">
			Repository <code>{route.repo}</code> not found. <a href="/repositories">Browse repositories</a>.
		</div>
	{:else if header && route.kind === 'code-graph'}
		<div class="scroll"><CodeGraphView repoId={header.id} /></div>
	{:else if header && route.kind === 'settings'}
		<div class="scroll"><RepoSettingsView repo={route.repo} /></div>
	{:else if header && !cloned}
		<div class="alert alert-info banner">
			{header.mirrorInfo.cloneInProgress
				? 'This repository is being cloned. Reload in a moment.'
				: 'This repository has not been cloned yet. It is queued and will be available soon.'}
		</div>
	{:else if header && emptyMessage && route.kind !== 'branches' && route.kind !== 'tags'}
		<div class="alert alert-info banner" role="status">{emptyMessage}</div>
	{:else if header}
		{#if browsing}
			<div class="browse">
				{#if treeShown}
					<aside
						class="sidebar"
						class:drawer={narrow}
						id="repo-file-tree"
						aria-label="File tree"
						style:width={narrow ? undefined : `${treeWidth}px`}
					>
						<FileTree
							repo={route.repo}
							urlRev={route.rev}
							activePath={route.path}
							activeIsDir={route.kind !== 'blob'}
						/>
						{#if !narrow}
							<!-- svelte-ignore a11y_no_noninteractive_tabindex, a11y_no_noninteractive_element_interactions -->
							<div
								class="resize-handle"
								role="separator"
								aria-orientation="vertical"
								aria-label="Resize file tree"
								aria-valuemin={TREE_MIN}
								aria-valuemax={TREE_MAX}
								aria-valuenow={treeWidth}
								tabindex="0"
								onpointerdown={onresizestart}
								onkeydown={onresizekey}
								ondblclick={() => setWidth(288)}
							></div>
						{/if}
					</aside>
					{#if narrow}
						<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
						<div class="tree-backdrop" aria-hidden="true" onclick={() => (treeDrawer = false)}></div>
					{/if}
				{/if}
				<section class="content">
					{#if route.kind === 'blob'}
						<FileView repo={route.repo} urlRev={route.rev} path={route.path} />
					{:else}
						<TreeView repo={route.repo} urlRev={route.rev} path={route.path} />
					{/if}
				</section>
			</div>
		{:else}
			<div class="scroll">
				{#if route.kind === 'commit'}
					<CommitView repo={route.repo} rev={route.path} />
				{:else if route.kind === 'commits'}
					<CommitList repo={route.repo} urlRev={route.rev} path={route.path} />
				{:else if route.kind === 'compare'}
					<CompareView repo={route.repo} spec={route.path} {defaultBranch} />
				{:else if route.kind === 'branches'}
					<RefsView repo={route.repo} type="GIT_BRANCH" />
				{:else if route.kind === 'tags'}
					<RefsView repo={route.repo} type="GIT_TAG" />
				{/if}
			</div>
		{/if}
	{:else}
		<p class="loading banner"><span class="spinner" aria-hidden="true"></span> Loading…</p>
	{/if}
</div>

<style>
	.repo-page {
		display: flex;
		flex-direction: column;
		height: calc(100vh - var(--nav-height));
		min-height: 0;
	}
	.repo-header {
		padding: var(--space-3) var(--space-4) 0;
		border-bottom: 1px solid var(--color-border);
		background: var(--color-bg);
	}
	.repo-title-row {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: var(--space-2);
		min-width: 0;
	}
	.tree-toggle {
		margin-left: calc(-1 * var(--space-1));
		color: var(--color-text-muted);
	}
	.repo-icon {
		display: inline-flex;
		color: var(--color-text-subtle);
	}
	.repo-name {
		font-size: var(--text-lg);
		font-weight: var(--weight-regular);
		margin: 0 var(--space-1) 0 0;
		overflow-wrap: anywhere;
		min-width: 0;
		letter-spacing: -0.01em;
	}
	.repo-name a {
		color: var(--color-text);
	}
	.repo-name a:hover {
		text-decoration: none;
	}
	.repo-name a:hover .repo-short {
		text-decoration: underline;
	}
	.repo-owner {
		color: var(--color-text-muted);
	}
	.repo-short {
		font-weight: var(--weight-semibold);
	}
	.repo-tabs {
		margin: var(--space-2) 0 -1px calc(-1 * var(--space-3));
		border-bottom: 0;
		max-width: calc(100% + var(--space-3));
	}
	.spacer {
		flex: 1;
	}
	.banner {
		margin: var(--space-4);
	}
	.browse {
		position: relative;
		flex: 1;
		display: flex;
		min-height: 0;
	}
	.sidebar {
		position: relative;
		flex: none;
		border-right: 1px solid var(--color-border);
		background: var(--color-bg-subtle);
		min-height: 0;
		padding-top: var(--space-1);
	}
	.sidebar.drawer {
		position: absolute;
		inset: 0 auto 0 0;
		z-index: 12;
		width: min(20rem, 85vw);
		box-shadow: var(--shadow-lg);
	}
	.tree-backdrop {
		position: absolute;
		inset: 0;
		z-index: 11;
		background: var(--overlay);
	}
	.resize-handle {
		position: absolute;
		top: 0;
		right: -3px;
		bottom: 0;
		width: 6px;
		cursor: col-resize;
		z-index: 2;
		touch-action: none;
	}
	.resize-handle::after {
		content: '';
		position: absolute;
		top: 0;
		bottom: 0;
		left: 2px;
		width: 2px;
		background: transparent;
		transition: background-color var(--duration) var(--ease);
	}
	.resize-handle:hover::after,
	.resize-handle:focus-visible::after {
		background: var(--color-primary);
	}
	.resize-handle:focus-visible {
		outline: none;
	}
	.content {
		flex: 1;
		min-width: 0;
		min-height: 0;
		overflow: hidden;
	}
	.scroll {
		flex: 1;
		min-height: 0;
		overflow: auto;
	}
	@media (max-width: 40rem) {
		.repo-header {
			padding: var(--space-2) var(--space-3) 0;
		}
	}
</style>
