<script lang="ts">
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import { relativeDate } from './blame.ts';
	import {
		cloneStateMessage,
		GIT_REFS,
		type GitRefNode,
		type GitRefsResult,
		type GitRefsVariables,
		type GitRefType
	} from './queries.ts';
	import { repoPageURL, repoURL } from './url.ts';

	interface Props {
		repo: string;
		type: GitRefType;
	}

	let { repo, type }: Props = $props();

	let refs = $state.raw<GitRefNode[]>([]);
	let total = $state(0);
	let defaultRef = $state<string | null>(null);
	let query = $state('');
	let loading = $state(true);
	let error = $state<string | null>(null);
	let seq = 0;

	const noun = $derived(type === 'GIT_BRANCH' ? 'branches' : 'tags');

	$effect(() => {
		query = '';
		void load(repo, type, '');
	});

	async function load(repoName: string, t: GitRefType, q: string): Promise<void> {
		const mine = ++seq;
		loading = true;
		error = null;
		try {
			const r = await graphql<GitRefsResult, GitRefsVariables>(GIT_REFS, {
				repo: repoName,
				type: t,
				first: 500,
				query: q.trim() || null,
				order: 'AUTHORED_OR_COMMITTED_AT'
			});
			if (mine !== seq) return;
			refs = r.repository?.gitRefs.nodes ?? [];
			total = r.repository?.gitRefs.totalCount ?? 0;
			defaultRef = r.repository?.defaultBranch?.name ?? null;
		} catch (err) {
			if (mine === seq) error = cloneStateMessage(err) ?? errorMessage(err);
		} finally {
			if (mine === seq) loading = false;
		}
	}

	let debounce: ReturnType<typeof setTimeout> | undefined;
	function oninput(): void {
		clearTimeout(debounce);
		debounce = setTimeout(() => load(repo, type, query), 200);
	}
</script>

<div class="refs-view">
	<div class="page-header">
		<h2>{type === 'GIT_BRANCH' ? 'Branches' : 'Tags'}</h2>
		<span class="muted">{total} {noun}</span>
	</div>
	<div class="field">
		<label class="visually-hidden" for="ref-filter">Filter {noun}</label>
		<input id="ref-filter" class="input" type="search" placeholder="Filter {noun}…" bind:value={query} {oninput} />
	</div>
	{#if error}
		<div class="alert alert-danger" role="alert">{error}</div>
	{/if}
	<table class="table" aria-busy={loading}>
		<tbody>
			{#each refs as r (r.name)}
				<tr>
					<td class="name">
						<a class="mono" href={repoURL(repo, r.displayName)}>{r.displayName}</a>
						{#if r.name === defaultRef}<span class="badge badge-primary">default</span>{/if}
					</td>
					<td class="subject muted">{r.target.commit?.subject ?? ''}</td>
					<td class="muted nowrap">
						{#if r.target.commit}
							{r.target.commit.author.person.name} · {relativeDate(r.target.commit.author.date)}
						{/if}
					</td>
					<td class="nowrap actions">
						<a class="mono" href={repoPageURL(repo, null, 'commit', r.target.oid)}>{r.target.abbreviatedOID}</a>
						{#if type === 'GIT_BRANCH' && r.name !== defaultRef}
							<a href={repoPageURL(repo, null, 'compare', `...${r.displayName}`)}>compare</a>
						{/if}
						<a href={repoPageURL(repo, r.displayName, 'commits')}>history</a>
					</td>
				</tr>
			{:else}
				<tr><td class="muted">{loading ? 'Loading…' : `No ${noun}.`}</td></tr>
			{/each}
		</tbody>
	</table>
</div>

<style>
	.refs-view {
		padding: var(--space-4);
		max-width: 80rem;
	}
	.name {
		white-space: nowrap;
	}
	.subject {
		overflow-wrap: anywhere;
	}
	.nowrap {
		white-space: nowrap;
	}
	.actions {
		text-align: right;
	}
	.actions a + a {
		margin-left: var(--space-2);
	}
</style>
