<script lang="ts">
	import Icon from '#lib/components/Icon.svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import OwnershipPanel from '#lib/own/OwnershipPanel.svelte';
	import { isMarkdownPath, pickReadme, README_BLOB, type ReadmeBlob, type ReadmeBlobResult } from './markdown.ts';
	import MarkdownView from './MarkdownView.svelte';
	import PathBreadcrumbs from './PathBreadcrumbs.svelte';
	import { cloneStateMessage, loadTree, type TreeEntryNode } from './queries.ts';
	import { parentPath, repoPageURL } from './url.ts';

	interface Props {
		repo: string;
		urlRev: string | null;
		path: string;
	}

	let { repo, urlRev, path }: Props = $props();

	const PREVIEW = 1000;

	let entries = $state.raw<TreeEntryNode[]>([]);
	let archiveURL = $state<string | null>(null);
	let missing = $state(false);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let showAll = $state(false);
	let showOwners = $state(false);
	let seq = 0;
	let readme = $state.raw<ReadmeBlob | null>(null);
	let readmeSeq = 0;

	$effect(() => {
		void load(repo, urlRev ?? 'HEAD', path);
	});

	async function load(repoName: string, rev: string, dir: string): Promise<void> {
		const mine = ++seq;
		loading = true;
		error = null;
		showAll = false;
		readme = null;
		readmeSeq++;
		try {
			const r = await loadTree(repoName, rev, dir);
			if (mine !== seq) return;
			const commit = r.repository?.commit;
			if (!commit) {
				error = `Revision ${rev} not found.`;
				entries = [];
				return;
			}
			missing = commit.tree === null;
			entries = commit.tree?.entries ?? [];
			archiveURL = commit.tree?.archiveURL ?? null;
			void loadReadme(repoName, rev, pickReadme(entries)?.path ?? null);
		} catch (err) {
			if (mine === seq) error = cloneStateMessage(err) ?? errorMessage(err);
		} finally {
			if (mine === seq) loading = false;
		}
	}

	const shown = $derived(showAll ? entries : entries.slice(0, PREVIEW));

	/** Loads the directory's README, rendered below the listing. */
	async function loadReadme(repoName: string, rev: string, readmePath: string | null): Promise<void> {
		const mine = ++readmeSeq;
		readme = null;
		if (readmePath === null) return;
		try {
			const r = await graphql<ReadmeBlobResult, { repo: string; rev: string; path: string }>(README_BLOB, {
				repo: repoName,
				rev,
				path: readmePath
			});
			const blob = r.repository?.commit?.blob ?? null;
			if (mine === readmeSeq) readme = blob && !blob.binary ? blob : null;
		} catch {
			// The listing is shown without the README.
		}
	}
</script>

<div class="tree-view">
	<div class="tree-header">
		<PathBreadcrumbs {repo} {urlRev} {path} isFile={false} />
		<div class="spacer"></div>
		<div class="toolbar">
			<button
				type="button"
				class="btn btn-sm"
				aria-pressed={showOwners}
				onclick={() => (showOwners = !showOwners)}><Icon name="users" size={14} /> Owners</button
			>
			<a class="btn btn-sm" href={repoPageURL(repo, urlRev, 'commits', path)}><Icon name="clock" size={14} /> History</a>
			{#if archiveURL}
				<a class="btn btn-sm" href={archiveURL} download>Download ZIP</a>
			{/if}
		</div>
	</div>

	{#if showOwners && !missing && !error}
		<OwnershipPanel {repo} rev={urlRev} {path} isDir={true} />
	{/if}

	{#if error}
		<div class="alert alert-danger" role="alert">{error}</div>
	{:else if missing}
		<div class="alert alert-warning">Directory not found at this revision.</div>
	{:else}
		<table class="table entries" aria-busy={loading}>
			<tbody>
				{#if path !== ''}
					<tr>
						<td class="mono"><a class="entry" href={repoPageURL(repo, urlRev, parentPath(path) === '' ? 'root' : 'tree', parentPath(path))}><span class="entry-icon"><Icon name="folder" size={15} /></span>..</a></td>
					</tr>
				{/if}
				{#each shown as e (e.path)}
					<tr>
						<td class="mono">
							<a class="entry" href={repoPageURL(repo, urlRev, e.isDirectory ? 'tree' : 'blob', e.path)} class:dir={e.isDirectory}>
								<span class="entry-icon"><Icon name={e.isDirectory ? 'folder' : 'file'} size={15} /></span>{e.name}{e.isDirectory ? '/' : ''}
							</a>
							{#if e.kind === 'SUBMODULE'}<span class="badge">submodule</span>{/if}
							{#if e.kind === 'SYMLINK'}<span class="badge">symlink</span>{/if}
						</td>
					</tr>
				{:else}
					<tr><td class="muted">{loading ? 'Loading…' : 'This directory is empty.'}</td></tr>
				{/each}
			</tbody>
		</table>
		{#if entries.length > shown.length}
			<p class="pad">
				<button type="button" class="btn btn-sm" onclick={() => (showAll = true)}>
					Show all {entries.length} entries
				</button>
			</p>
		{/if}
		{#if readme}
			<section class="readme" aria-label={readme.name}>
				<header class="readme-header">
					<Icon name="file" size={15} /><a href={readme.url}>{readme.name}</a>
				</header>
				<div class="readme-body">
					{#if isMarkdownPath(readme.path)}
						<MarkdownView source={readme.content} {repo} rev={urlRev} path={readme.path} />
					{:else}
						<pre class="readme-text">{readme.content}</pre>
					{/if}
					{#if readme.contentTruncated}
						<p class="muted">This README is too large to show in full. <a href={readme.url}>Open the file</a>.</p>
					{/if}
				</div>
			</section>
		{/if}
	{/if}
</div>

<style>
	.tree-view {
		height: 100%;
		overflow: auto;
	}
	.tree-header {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: var(--space-2) var(--space-3);
		min-height: 2.75rem;
		padding: var(--space-2) var(--space-4);
		border-bottom: 1px solid var(--color-border);
		background: var(--color-bg-subtle);
		position: sticky;
		top: 0;
		z-index: 1;
	}
	.toolbar {
		display: flex;
		flex-wrap: wrap;
		gap: var(--space-1);
	}
	.toolbar :global(.icon) {
		color: var(--color-text-muted);
	}
	.spacer {
		flex: 1;
	}
	.entries {
		margin: 0;
	}
	.entries td {
		padding: 0.35rem var(--space-4);
		font-size: var(--text-sm);
	}
	.entries tr:hover td {
		background: var(--color-bg-hover);
	}
	.entry {
		display: inline-flex;
		align-items: center;
		gap: var(--space-2);
		color: var(--color-text);
	}
	.entry:hover {
		color: var(--color-link);
	}
	.entry-icon {
		display: inline-flex;
		color: var(--color-text-subtle);
	}
	.dir .entry-icon {
		color: var(--color-primary);
		opacity: 0.8;
	}
	.dir {
		font-weight: var(--weight-medium);
	}
	.pad {
		padding: var(--space-3) var(--space-4);
	}
	.readme {
		margin: var(--space-4);
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
	}
	.readme-header {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		padding: var(--space-2) var(--space-4);
		border-bottom: 1px solid var(--color-border);
		background: var(--color-bg-subtle);
		font-size: var(--text-sm);
		font-weight: var(--weight-medium);
		color: var(--color-text-subtle);
	}
	.readme-header a {
		color: var(--color-text);
	}
	.readme-body {
		padding: var(--space-4) var(--space-6);
	}
	.readme-text {
		margin: 0;
		white-space: pre-wrap;
		font-size: var(--text-sm);
	}
	.alert {
		margin: var(--space-4);
	}
</style>
