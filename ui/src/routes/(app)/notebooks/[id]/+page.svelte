<script lang="ts">
	import type { UpdateNotebookMutation } from '#lib/graphql-types.ts';
	import { logEvent } from '#lib/events.ts';
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { untrack } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import NotebookBlock from '#lib/notebooks/NotebookBlock.svelte';
	import {
		CREATE_NOTEBOOK_STAR,
		DELETE_NOTEBOOK,
		DELETE_NOTEBOOK_STAR,
		NOTEBOOK,
		UPDATE_NOTEBOOK,
		blockToInput,
		inputToBlock,
		namespaceLabel,
		newBlockID,
		type NotebookInputVariable,
		type NotebookNode,
		type NotebookResult
	} from '#lib/notebooks/queries.ts';
	import {
		DEFAULT_LINE_CONTEXT,
		notebookFileName,
		serializeNotebook,
		type Block,
		type ResolvedSymbol
	} from '#lib/notebooks/serialize.ts';
	import { VIEWER_NAMESPACES, type ViewerNamespacesResult } from '#lib/search/queries.ts';

	const id = $derived(page.params.id ?? '');
	const user = $derived(page.data.auth?.user ?? null);

	let notebook = $state.raw<NotebookNode | null>(null);
	let blocks = $state.raw<Block[]>([]);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let editing = $state<Set<string>>(new Set());
	const resolved = new Map<string, ResolvedSymbol>();
	let saving = $state(false);
	let saveError = $state<string | null>(null);
	let titleDraft = $state<string | null>(null);
	let namespaces = $state.raw<{ id: string; label: string }[]>([]);
	let showSharing = $state(false);

	async function load(nodeId: string): Promise<void> {
		loading = true;
		error = null;
		try {
			const r = await graphql<NotebookResult, { id: string }>(NOTEBOOK, { id: nodeId });
			notebook = r.notebook;
			blocks = r.notebook ? r.notebook.blocks.map(inputToBlock) : [];
			if (!r.notebook) error = 'Notebook not found.';
			// A new notebook opens with its first block in the editor.
			if (r.notebook?.viewerCanManage && page.url.searchParams.get('edit') && blocks[0]) {
				editing = new Set([blocks[0].id]);
			}
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loading = false;
		}
	}

	$effect(() => {
		const nodeId = id;
		untrack(() => void load(nodeId));
	});

	async function loadNamespaces(): Promise<void> {
		try {
			const r = await graphql<ViewerNamespacesResult>(VIEWER_NAMESPACES);
			const u = r.currentUser;
			if (!u) return;
			namespaces = [
				{ id: u.id, label: `@${u.username}` },
				...u.organizations.nodes.map((o) => ({ id: o.id, label: `@${o.name}` }))
			];
		} catch {
			namespaces = [];
		}
	}

	// Saves run one after another, each with the state at the time it starts.
	let queue: Promise<void> = Promise.resolve();
	function save(patch: Partial<Pick<NotebookInputVariable, 'title' | 'public' | 'namespace'>> = {}): Promise<void> {
		queue = queue.then(async () => {
			const nb = notebook;
			if (!nb?.viewerCanManage || !nb.namespace) return;
			saving = true;
			saveError = null;
			try {
				const input: NotebookInputVariable = {
					title: nb.title,
					public: nb.public,
					namespace: nb.namespace.id,
					// File and symbol blocks still being filled in are not saved yet.
					blocks: blocks.filter(isComplete).map(blockToInput),
					...patch
				};
				const r = await graphql<UpdateNotebookMutation, { id: string; notebook: NotebookInputVariable }>(
					UPDATE_NOTEBOOK,
					{ id: nb.id, notebook: input }
				);
				notebook = r.updateNotebook;
			} catch (err) {
				saveError = errorMessage(err);
			} finally {
				saving = false;
			}
		});
		return queue;
	}

	function isComplete(b: Block): boolean {
		if (b.type === 'file') return !!b.input.repositoryName && !!b.input.filePath;
		if (b.type === 'symbol') return !!b.input.repositoryName && !!b.input.filePath && !!b.input.symbolName;
		return true;
	}

	function setBlocks(next: Block[]): void {
		blocks = next;
		void save();
	}

	function setEditing(blockId: string, on: boolean): void {
		const next = new Set(editing);
		if (on) next.add(blockId);
		else next.delete(blockId);
		editing = next;
	}

	function addBlock(type: Block['type']): void {
		const blockId = newBlockID();
		let b: Block;
		switch (type) {
			case 'md':
				b = { id: blockId, type, input: { text: '' } };
				break;
			case 'query':
				b = { id: blockId, type, input: { query: '' } };
				break;
			case 'file':
				b = { id: blockId, type, input: { repositoryName: '', revision: '', filePath: '', lineRange: null } };
				break;
			case 'symbol':
				b = {
					id: blockId,
					type,
					input: {
						repositoryName: '',
						revision: '',
						filePath: '',
						symbolName: '',
						symbolContainerName: '',
						symbolKind: 'UNKNOWN',
						lineContext: DEFAULT_LINE_CONTEXT
					}
				};
				break;
		}
		// Not saved until it has content: an empty file or symbol block would not validate.
		blocks = [...blocks, b];
		setEditing(blockId, true);
		if (type === 'md' || type === 'query') void save();
	}

	function changeBlock(b: Block): void {
		setBlocks(blocks.map((x) => (x.id === b.id ? b : x)));
	}

	function moveBlock(index: number, delta: number): void {
		const j = index + delta;
		if (j < 0 || j >= blocks.length) return;
		const next = [...blocks];
		[next[index], next[j]] = [next[j]!, next[index]!];
		setBlocks(next);
	}

	function duplicateBlock(index: number): void {
		const b = blocks[index];
		if (!b) return;
		const copy = { ...structuredClone(b), id: newBlockID() } as Block;
		setBlocks([...blocks.slice(0, index + 1), copy, ...blocks.slice(index + 1)]);
	}

	function deleteBlock(index: number): void {
		const b = blocks[index];
		if (!b) return;
		setEditing(b.id, false);
		setBlocks(blocks.filter((_, i) => i !== index));
	}

	/** Leaves editing; a never-filled file or symbol block is dropped. */
	function stopEditing(b: Block): void {
		setEditing(b.id, false);
		const current = blocks.find((x) => x.id === b.id);
		if (current && !isComplete(current)) blocks = blocks.filter((x) => x.id !== b.id);
	}

	async function saveTitle(): Promise<void> {
		const t = titleDraft?.trim();
		titleDraft = null;
		if (!notebook || !t || t === notebook.title) return;
		await save({ title: t });
	}

	async function toggleStar(): Promise<void> {
		const nb = notebook;
		if (!nb || !user) return;
		try {
			await graphql<unknown, { id: string }>(nb.viewerHasStarred ? DELETE_NOTEBOOK_STAR : CREATE_NOTEBOOK_STAR, { id: nb.id });
			notebook = {
				...nb,
				viewerHasStarred: !nb.viewerHasStarred,
				stars: { totalCount: nb.stars.totalCount + (nb.viewerHasStarred ? -1 : 1) }
			};
		} catch (err) {
			saveError = errorMessage(err);
		}
	}

	async function remove(): Promise<void> {
		const nb = notebook;
		if (!nb || !confirm(`Delete the notebook “${nb.title}”? This cannot be undone.`)) return;
		try {
			await graphql<unknown, { id: string }>(DELETE_NOTEBOOK, { id: nb.id });
			await goto('/notebooks');
		} catch (err) {
			saveError = errorMessage(err);
		}
	}

	function exportNotebook(): void {
		const nb = notebook;
		if (!nb) return;
		const text = serializeNotebook(blocks, location.origin, resolved);
		const url = URL.createObjectURL(new Blob([text], { type: 'text/markdown' }));
		const a = document.createElement('a');
		a.href = url;
		a.download = notebookFileName(nb.title);
		a.click();
		setTimeout(() => URL.revokeObjectURL(url), 1000);
	}

	function formatDate(s: string): string {
		return new Date(s).toLocaleString(undefined, { dateStyle: 'medium', timeStyle: 'short' });
	}

	// A usage event for the admin analytics.
	$effect(() => {
		if (page.params.id) logEvent('SearchNotebookPageViewed');
	});
</script>

<svelte:head><title>{notebook?.title ?? 'Notebook'} · Sourcegull</title></svelte:head>

<div class="page notebook-page">
	{#if loading && !notebook}
		<p class="muted">Loading…</p>
	{:else if error || !notebook}
		<div class="alert alert-danger" role="alert">{error ?? 'Notebook not found.'}</div>
		<a href="/notebooks">All notebooks</a>
	{:else}
		<p class="crumbs"><a href="/notebooks">Notebooks</a> › {namespaceLabel(notebook.namespace)}</p>
		<div class="page-header">
			<div class="heading">
				{#if titleDraft !== null}
					<form
						class="title-form"
						onsubmit={(e) => {
							e.preventDefault();
							void saveTitle();
						}}
					>
						<label class="visually-hidden" for="nb-title">Title</label>
						<!-- svelte-ignore a11y_autofocus -->
						<input id="nb-title" class="input title-input" bind:value={titleDraft} autofocus onblur={saveTitle} />
					</form>
				{:else}
					<h1>
						{notebook.title}
						<span class="badge" class:badge-primary={notebook.public}>{notebook.public ? 'public' : 'private'}</span>
						{#if notebook.viewerCanManage}
							<button type="button" class="btn btn-sm btn-ghost" onclick={() => (titleDraft = notebook?.title ?? '')}
								>Rename</button
							>
						{/if}
					</h1>
				{/if}
				<p class="page-description">
					{#if notebook.creator}Created by {notebook.creator.username} ·{/if}
					updated {formatDate(notebook.updatedAt)}{#if notebook.updater} by {notebook.updater.username}{/if}
					{#if saving}· saving…{/if}
				</p>
			</div>
			<div class="page-actions">
				{#if user}
					<button type="button" class="btn btn-sm" aria-pressed={notebook.viewerHasStarred} onclick={toggleStar}>
						{notebook.viewerHasStarred ? '★ Starred' : '☆ Star'} · {notebook.stars.totalCount}
					</button>
				{:else}
					<span class="muted">★ {notebook.stars.totalCount}</span>
				{/if}
				<button type="button" class="btn btn-sm" onclick={exportNotebook}>Export</button>
				{#if notebook.viewerCanManage}
					<button
						type="button"
						class="btn btn-sm"
						aria-expanded={showSharing}
						onclick={() => {
							showSharing = !showSharing;
							if (showSharing && namespaces.length === 0) void loadNamespaces();
						}}>Share</button
					>
					<button type="button" class="btn btn-sm btn-danger" onclick={remove}>Delete</button>
				{/if}
			</div>
		</div>

		{#if showSharing && notebook.viewerCanManage}
			<section class="card sharing" aria-label="Sharing">
				<label>
					Owner
					<select
						class="input"
						value={notebook.namespace?.id}
						onchange={(e) => save({ namespace: (e.currentTarget as HTMLSelectElement).value })}
					>
						{#if notebook.namespace && !namespaces.some((n) => n.id === notebook?.namespace?.id)}
							<option value={notebook.namespace.id}>{namespaceLabel(notebook.namespace)}</option>
						{/if}
						{#each namespaces as n (n.id)}<option value={n.id}>{n.label}</option>{/each}
					</select>
				</label>
				<label class="checkbox">
					<input
						type="checkbox"
						checked={notebook.public}
						onchange={(e) => save({ public: (e.currentTarget as HTMLInputElement).checked })}
					/>
					Public: every user of this instance can see it
				</label>
				<p class="muted hint">
					Private notebooks are visible to their owner{notebook.namespace?.__typename === 'Org'
						? ', here the members of the organization'
						: ''}. Blocks showing code from repositories a reader may not access are hidden from them.
				</p>
			</section>
		{/if}

		{#if saveError}<div class="alert alert-danger" role="alert">{saveError}</div>{/if}

		<div class="blocks">
			{#each blocks as b, i (b.id)}
				<NotebookBlock
					block={b}
					index={i}
					count={blocks.length}
					canManage={notebook.viewerCanManage}
					patternType={notebook.patternType}
					editing={editing.has(b.id)}
					onedit={(on) => (on ? setEditing(b.id, true) : stopEditing(b))}
					onchange={changeBlock}
					onmove={(d) => moveBlock(i, d)}
					onduplicate={() => duplicateBlock(i)}
					ondelete={() => deleteBlock(i)}
					onresolved={(r) => {
						if (r) resolved.set(b.id, r);
						else resolved.delete(b.id);
					}}
				/>
			{:else}
				<div class="empty-state">
					<h3>This notebook is empty</h3>
					{#if notebook.viewerCanManage}<p>Add a Markdown, query, file or symbol block below.</p>{/if}
				</div>
			{/each}
		</div>

		{#if notebook.viewerCanManage}
			<div class="add" role="group" aria-label="Add a block">
				<span class="muted">Add:</span>
				<button type="button" class="btn btn-sm" onclick={() => addBlock('md')}>Markdown</button>
				<button type="button" class="btn btn-sm" onclick={() => addBlock('query')}>Query</button>
				<button type="button" class="btn btn-sm" onclick={() => addBlock('file')}>File</button>
				<button type="button" class="btn btn-sm" onclick={() => addBlock('symbol')}>Symbol</button>
			</div>
		{/if}
	{/if}
</div>

<style>
	.notebook-page {
		max-width: 64rem;
	}
	.heading {
		flex: 1 1 24rem;
		min-width: 0;
	}
	.page-header h1 {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: var(--space-2);
	}
	.title-input {
		font-size: var(--text-xl);
		width: 100%;
	}
	.sharing {
		display: grid;
		gap: var(--space-2);
		padding: var(--space-3);
		margin-bottom: var(--space-4);
	}
	.sharing label {
		display: flex;
		gap: var(--space-2);
		align-items: center;
	}
	.sharing select {
		width: auto;
	}
	.hint {
		margin: 0;
		font-size: var(--text-sm);
	}
	.blocks {
		display: grid;
		gap: var(--space-2);
	}
	.add {
		display: flex;
		gap: var(--space-2);
		align-items: center;
		margin-top: var(--space-4);
		padding-top: var(--space-3);
		border-top: 1px dashed var(--color-border-strong);
	}
</style>
