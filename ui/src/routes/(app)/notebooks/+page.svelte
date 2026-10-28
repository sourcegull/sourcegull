<script lang="ts">
	import type { CreateNotebookMutation } from '#lib/graphql-types.ts';
	import { goto } from '$app/navigation';
	import Icon from '#lib/components/Icon.svelte';
	import { page } from '$app/state';
	import { onMount, untrack } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		CREATE_NOTEBOOK,
		NOTEBOOKS,
		blockToInput,
		namespaceLabel,
		newBlockID,
		notebookURL,
		type NotebookInputVariable,
		type NotebookSummary,
		type NotebooksOrderBy,
		type NotebooksResult,
		type NotebooksVariables
	} from '#lib/notebooks/queries.ts';
	import { markdownToBlocks, titleFromFileName, type Block } from '#lib/notebooks/serialize.ts';
	import { VIEWER_NAMESPACES, type ViewerNamespacesResult } from '#lib/search/queries.ts';

	const PAGE_SIZE = 50;

	const user = $derived(page.data.auth?.user ?? null);
	/** `explore`, `mine`, `starred` or `org:<id>`. */
	const tab = $derived(page.url.searchParams.get('tab') ?? 'explore');

	let orgs = $state.raw<{ id: string; name: string }[]>([]);
	let items = $state.raw<NotebookSummary[]>([]);
	let totalCount = $state(0);
	let cursor = $state<string | null>(null);
	let hasMore = $state(false);
	let filter = $state(page.url.searchParams.get('query') ?? '');
	let orderBy = $state<NotebooksOrderBy>('NOTEBOOK_UPDATED_AT');
	let loading = $state(true);
	let error = $state<string | null>(null);
	let busy = $state(false);
	let actionError = $state<string | null>(null);
	let seq = 0;

	function tabURL(t: string): string {
		const p = new URLSearchParams(page.url.search);
		if (t === 'explore') p.delete('tab');
		else p.set('tab', t);
		const s = p.toString();
		return s ? `/notebooks?${s}` : '/notebooks';
	}

	function variables(after: string | null): NotebooksVariables {
		const v: NotebooksVariables = {
			first: PAGE_SIZE,
			after,
			query: filter.trim() || null,
			orderBy,
			descending: true
		};
		if (tab === 'mine' && user) v.creatorUserID = user.id;
		else if (tab === 'starred' && user) v.starredByUserID = user.id;
		else if (tab.startsWith('org:')) v.namespace = tab.slice(4);
		return v;
	}

	async function load(after: string | null): Promise<void> {
		const mine = ++seq;
		loading = true;
		error = null;
		try {
			const r = await graphql<NotebooksResult, NotebooksVariables>(NOTEBOOKS, variables(after));
			if (mine !== seq) return;
			items = after ? [...items, ...r.notebooks.nodes] : r.notebooks.nodes;
			totalCount = r.notebooks.totalCount;
			cursor = r.notebooks.pageInfo.endCursor;
			hasMore = r.notebooks.pageInfo.hasNextPage;
		} catch (err) {
			if (mine === seq) error = errorMessage(err);
		} finally {
			if (mine === seq) loading = false;
		}
	}

	// Reload when the tab changes.
	$effect(() => {
		void tab;
		untrack(() => void load(null));
	});

	let debounce: ReturnType<typeof setTimeout> | undefined;
	function oninput(): void {
		clearTimeout(debounce);
		debounce = setTimeout(() => load(null), 200);
	}

	onMount(() => {
		if (user) {
			graphql<ViewerNamespacesResult>(VIEWER_NAMESPACES)
				.then((r) => (orgs = r.currentUser?.organizations.nodes ?? []))
				.catch(() => (orgs = []));
		}
		return () => clearTimeout(debounce);
	});

	async function create(title: string, blocks: Block[]): Promise<void> {
		if (!user) return;
		busy = true;
		actionError = null;
		try {
			const r = await graphql<CreateNotebookMutation, { notebook: NotebookInputVariable }>(
				CREATE_NOTEBOOK,
				{ notebook: { title, namespace: user.id, public: false, blocks: blocks.map(blockToInput) } }
			);
			await goto(`${notebookURL(r.createNotebook.id)}${blocks.length === 1 ? '?edit=1' : ''}`);
		} catch (err) {
			actionError = errorMessage(err);
		} finally {
			busy = false;
		}
	}

	function createEmpty(): void {
		void create('New notebook', [{ id: newBlockID(), type: 'md', input: { text: '' } }]);
	}

	let fileInput = $state<HTMLInputElement>();

	async function importFile(e: Event): Promise<void> {
		const input = e.currentTarget as HTMLInputElement;
		const file = input.files?.[0];
		input.value = '';
		if (!file) return;
		try {
			const text = await file.text();
			const blocks = markdownToBlocks(text).map((b) => ({ ...b, id: newBlockID() }) as Block);
			await create(titleFromFileName(file.name), blocks);
		} catch (err) {
			actionError = `Import failed: ${errorMessage(err)}`;
		}
	}

	function formatDate(s: string): string {
		return new Date(s).toLocaleDateString(undefined, { year: 'numeric', month: 'short', day: 'numeric' });
	}
</script>

<svelte:head><title>Notebooks · Sourcegull</title></svelte:head>

<div class="page">
	<div class="page-header">
		<div>
			<h1>Notebooks</h1>
			<p class="page-description">
				Markdown mixed with live search, file and symbol blocks: document how code works, write onboarding guides or keep
				a set of searches together. Export and import them as <code class="code-inline">.snb.md</code> files.
			</p>
		</div>
		{#if user}
			<div class="page-actions">
				<button type="button" class="btn" disabled={busy} onclick={() => fileInput?.click()}>
					<Icon name="upload" size={16} />Import
				</button>
				<input
					bind:this={fileInput}
					class="visually-hidden"
					type="file"
					accept=".md,.snb.md,text/markdown"
					aria-label="Import a notebook from a .snb.md file"
					onchange={importFile}
				/>
				<button type="button" class="btn btn-primary" disabled={busy} onclick={createEmpty}>
					<Icon name="plus" size={16} />New notebook
				</button>
			</div>
		{/if}
	</div>

	{#if actionError}<div class="alert alert-danger" role="alert">{actionError}</div>{/if}

	<nav class="tabs" aria-label="Notebooks">
		<a href={tabURL('explore')} aria-current={tab === 'explore' ? 'page' : undefined}>Explore</a>
		{#if user}
			<a href={tabURL('mine')} aria-current={tab === 'mine' ? 'page' : undefined}>My notebooks</a>
			<a href={tabURL('starred')} aria-current={tab === 'starred' ? 'page' : undefined}>Starred</a>
			{#each orgs as o (o.id)}
				<a href={tabURL(`org:${o.id}`)} aria-current={tab === `org:${o.id}` ? 'page' : undefined}>@{o.name}</a>
			{/each}
		{/if}
	</nav>

	<div class="filter-bar">
		<label class="visually-hidden" for="nb-filter">Search notebooks</label>
		<input
			id="nb-filter"
			class="input"
			type="search"
			placeholder="Search titles and content…"
			bind:value={filter}
			{oninput}
		/>
		<label class="visually-hidden" for="nb-order">Sort by</label>
		<select id="nb-order" class="input" bind:value={orderBy} onchange={() => load(null)}>
			<option value="NOTEBOOK_UPDATED_AT">Recently updated</option>
			<option value="NOTEBOOK_CREATED_AT">Recently created</option>
			<option value="NOTEBOOK_STAR_COUNT">Most stars</option>
		</select>
		<span class="muted small count">{totalCount} {totalCount === 1 ? 'notebook' : 'notebooks'}</span>
	</div>

	{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}

	{#if items.length === 0 && !loading && !error && !filter.trim()}
		<div class="empty-state">
			<span class="icon"><Icon name="notebook" size={32} /></span>
			<h3>{tab === 'starred' ? 'No starred notebooks' : 'No notebooks yet'}</h3>
			<p>
				{tab === 'starred'
					? 'Star a notebook to keep it here.'
					: 'Write down how a part of the code works, with searches and code that stay up to date.'}
			</p>
			{#if user && tab !== 'starred'}
				<button type="button" class="btn btn-primary" disabled={busy} onclick={createEmpty}>
					<Icon name="plus" size={16} />Create notebook
				</button>
			{/if}
		</div>
	{:else}
		<ul class="list list-group" aria-busy={loading}>
			{#each items as n (n.id)}
				<li>
					<div class="list-item-title">
						<a href={notebookURL(n.id)}>{n.title}</a>
						<span class="badge" class:badge-primary={n.public}>{n.public ? 'public' : 'private'}</span>
						<span class="push muted stars" title="Stars"><Icon name="star" size={14} />{n.stars.totalCount}</span>
					</div>
					<p class="list-item-meta">
						{namespaceLabel(n.namespace)}
						{#if n.creator}· created by {n.creator.username}{/if}
						· updated {formatDate(n.updatedAt)}
					</p>
				</li>
			{:else}
				<li class="empty">{loading ? 'Loading…' : 'No notebooks match.'}</li>
			{/each}
		</ul>
	{/if}

	{#if hasMore}
		<div class="more">
			<button type="button" class="btn" disabled={loading} onclick={() => load(cursor)}>
				{loading ? 'Loading…' : 'Show more'}
			</button>
		</div>
	{/if}
</div>

<style>
	.count {
		margin-left: auto;
	}
	.stars {
		font-size: var(--text-sm);
		gap: var(--space-1);
	}
	.more {
		margin-top: var(--space-4);
		text-align: center;
	}
</style>
