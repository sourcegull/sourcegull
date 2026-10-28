<script lang="ts">
	import Icon from '#lib/components/Icon.svelte';
	import { goto } from '$app/navigation';
	import { onMount } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		CREATE_TEAM,
		TEAMS,
		teamUrl,
		type CreateTeamResult,
		type TeamSummary,
		type TeamsResult,
		type Vars
} from '#lib/orgs/queries.ts';

	const PAGE_SIZE = 50;

	let teams = $state<TeamSummary[]>([]);
	let totalCount = $state(0);
	let endCursor = $state<string | null>(null);
	let search = $state('');
	let loading = $state(true);
	let error = $state<string | null>(null);
	let creating = $state(false);
	let name = $state('');
	let displayName = $state('');
	let parent = $state('');
	let busy = $state(false);
	let requestSeq = 0;

	async function load(more = false): Promise<void> {
		const seq = ++requestSeq;
		loading = true;
		try {
			const s = search.trim();
			const result = await graphql<TeamsResult, Vars>(TEAMS, {
				first: PAGE_SIZE,
				after: more ? endCursor : null,
				search: s || null,
				// Searching finds child teams too; the plain list shows the top of the hierarchy.
				includeChildTeams: s !== ''
			});
			if (seq !== requestSeq) return;
			teams = more ? [...teams, ...result.teams.nodes] : result.teams.nodes;
			totalCount = result.teams.totalCount;
			endCursor = result.teams.pageInfo.hasNextPage ? result.teams.pageInfo.endCursor : null;
			error = null;
		} catch (err) {
			if (seq === requestSeq) error = errorMessage(err);
		} finally {
			if (seq === requestSeq) loading = false;
		}
	}

	let debounce: ReturnType<typeof setTimeout> | undefined;
	function oninput(): void {
		clearTimeout(debounce);
		debounce = setTimeout(() => load(), 200);
	}

	async function create(): Promise<void> {
		busy = true;
		error = null;
		try {
			const result = await graphql<CreateTeamResult, Vars>(CREATE_TEAM, {
				name: name.trim(),
				displayName: displayName.trim() || null,
				parentTeamName: parent.trim() || null
			});
			await goto(teamUrl(result.createTeam.name));
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = false;
		}
	}

	onMount(() => {
		void load();
		return () => clearTimeout(debounce);
	});
</script>

<svelte:head><title>Teams · Sourcegull</title></svelte:head>

<div class="page">
	<div class="page-header">
		<div>
			<h1>Teams</h1>
			<p class="page-description">
				Teams own code (CODEOWNERS <code class="code-inline">@team</code> handles and assigned ownership). They can be
				nested, and are synced from GitLab groups when <code class="code-inline">own.syncTeamsFromGitLabGroups</code> is on.
			</p>
		</div>
		<div class="page-actions">
			<button class="btn btn-primary" type="button" onclick={() => (creating = !creating)}
				><Icon name="plus" size={16} />New team</button
			>
		</div>
	</div>

	{#if creating}
		<form
			class="card create"
			onsubmit={(event) => {
				event.preventDefault();
				void create();
			}}
		>
			<div class="field">
				<label class="label" for="team-name">Name</label>
				<input id="team-name" class="input" bind:value={name} required placeholder="acme/backend" />
			</div>
			<div class="field">
				<label class="label" for="team-display">Display name</label>
				<input id="team-display" class="input" bind:value={displayName} placeholder="Backend" />
			</div>
			<div class="field">
				<label class="label" for="team-parent">Parent team (optional)</label>
				<input id="team-parent" class="input" bind:value={parent} placeholder="acme" />
			</div>
			<button class="btn btn-primary" type="submit" disabled={busy || !name.trim()}>Create team</button>
		</form>
	{/if}

	<div class="filter-bar">
		<label class="visually-hidden" for="team-filter">Search teams</label>
		<input
			id="team-filter"
			class="input"
			type="search"
			placeholder="Search teams…"
			bind:value={search}
			{oninput}
		/>
		<span class="muted small count">{totalCount} {search.trim() ? 'matching' : 'top-level'}</span>
	</div>

	{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}

	<div class="table-scroll">
		<table class="table table-bordered" aria-busy={loading}>
			<thead>
				<tr>
					<th scope="col">Team</th>
					<th scope="col">Members</th>
					<th scope="col">Child teams</th>
					<th scope="col"><span class="visually-hidden">Flags</span></th>
				</tr>
			</thead>
			<tbody>
				{#each teams as team (team.id)}
					<tr>
						<td>
							<a href={teamUrl(team.name)}><strong>{team.name}</strong></a>
							{#if team.displayName}<span class="muted">{team.displayName}</span>{/if}
						</td>
						<td>{team.members.totalCount}</td>
						<td>{team.childTeams.totalCount}</td>
						<td>
							{#if team.syncedFromCodeHost}<span class="badge" title="Synced from a GitLab group">GitLab</span
								>{:else if team.readonly}<span class="badge">Read-only</span>{/if}
						</td>
					</tr>
				{:else}
					{#if !loading}<tr><td colspan="4" class="muted empty">{search.trim() ? 'No teams match.' : 'No teams yet. Create one, or turn on syncing from GitLab groups.'}</td></tr>{/if}
				{/each}
			</tbody>
		</table>
	</div>
	{#if endCursor}
		<button class="btn" type="button" disabled={loading} onclick={() => load(true)}>Show more</button>
	{/if}
</div>

<style>
	.count {
		margin-left: auto;
	}
	.empty {
		text-align: center;
		padding: var(--space-6);
	}
	.create {
		max-width: 32rem;
		margin-bottom: var(--space-4);
	}
</style>
