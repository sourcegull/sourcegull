<script lang="ts">
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import { relativeDate } from './blame.ts';
	import DiffView from './DiffView.svelte';
	import { cloneStateMessage, COMMIT, type CommitDetail, type CommitResult } from './queries.ts';
	import { repoPageURL, repoURL } from './url.ts';

	interface Props {
		repo: string;
		rev: string;
	}

	let { repo, rev }: Props = $props();

	let commit = $state.raw<CommitDetail | null>(null);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let seq = 0;

	$effect(() => {
		void load(repo, rev);
	});

	async function load(repoName: string, r: string): Promise<void> {
		const mine = ++seq;
		loading = true;
		error = null;
		try {
			const res = await graphql<CommitResult, { repo: string; rev: string }>(COMMIT, {
				repo: repoName,
				rev: r
			});
			if (mine !== seq) return;
			commit = res.repository?.commit ?? null;
			if (!commit) error = `Commit ${r} not found.`;
		} catch (err) {
			if (mine === seq) error = cloneStateMessage(err) ?? errorMessage(err);
		} finally {
			if (mine === seq) loading = false;
		}
	}
</script>

<svelte:head><title>{commit ? `${commit.subject} · ` : ''}{repo} · Sourcegull</title></svelte:head>

<div class="commit-view">
	{#if error}
		<div class="alert alert-danger" role="alert">{error}</div>
	{:else if loading && !commit}
		<p class="loading"><span class="spinner" aria-hidden="true"></span> Loading…</p>
	{:else if commit}
		<div class="card commit-header">
			<h1>{commit.subject}</h1>
			{#if commit.body}<pre class="body">{commit.body}</pre>{/if}
			<div class="meta">
				<span>
					<strong>{commit.author.person.name}</strong>
					<span class="muted">authored</span>
					<time datetime={commit.author.date} title={commit.author.date}>{relativeDate(commit.author.date)}</time>
				</span>
				{#if commit.committer && commit.committer.person.name !== commit.author.person.name}
					<span>
						<span class="muted">committed by</span> <strong>{commit.committer.person.name}</strong>
					</span>
				{/if}
				<span class="spacer"></span>
				<span class="muted">commit</span> <code class="code-inline oid">{commit.oid}</code>
			</div>
			<div class="meta">
				{#if commit.parents.length > 0}
					<span class="muted">{commit.parents.length === 1 ? 'parent' : 'parents'}</span>
					{#each commit.parents as p (p.oid)}
						<a class="mono" href={p.url}>{p.abbreviatedOID}</a>
					{/each}
				{:else}
					<span class="muted">root commit</span>
				{/if}
				<span class="spacer"></span>
				<a class="btn btn-sm" href={repoURL(repo, commit.oid)}>Browse files</a>
				<a class="btn btn-sm" href={repoPageURL(repo, commit.oid, 'commits')}>History</a>
			</div>
		</div>
		<DiffView {repo} diffs={commit.diff.fileDiffs} headRev={commit.oid} />
	{/if}
</div>

<style>
	.commit-view {
		padding: var(--space-5) var(--space-4);
		max-width: 90rem;
	}
	.commit-header {
		padding: var(--space-4) var(--space-5);
	}
	.commit-header h1 {
		font-size: var(--text-xl);
		font-weight: var(--weight-semibold);
		letter-spacing: -0.01em;
		margin: 0 0 var(--space-2);
		overflow-wrap: anywhere;
	}
	.oid {
		overflow-wrap: anywhere;
	}
	.body {
		white-space: pre-wrap;
		margin: 0 0 var(--space-3);
		color: var(--color-text-muted);
	}
	.meta {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: var(--space-2);
		margin-top: var(--space-2);
		font-size: var(--text-sm);
	}
	.body {
		font-size: var(--text-sm);
	}
	.spacer {
		flex: 1;
	}
</style>
