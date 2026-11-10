<script lang="ts">
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { untrack } from 'svelte';
	import Pager from '#lib/components/Pager.svelte';
	import { cursorPages, firstPage, hasPreviousPage, pageAfter, pageLoaded, previousPage } from '#lib/cursor-pages.ts';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		ADD_TEAM_MEMBERS,
		DELETE_TEAM,
		REMOVE_TEAM_MEMBERS,
		TEAM,
		TEAM_MEMBERS,
		UPDATE_TEAM,
		parseUsernames,
		teamUrl,
		type MemberNode,
		type TeamDetail,
		type TeamMembersResult,
		type TeamResult,
		type Vars
	} from '#lib/orgs/queries.ts';

	const name = $derived(page.params.name ?? '');
	let team = $state<TeamDetail | null>(null);
	let loaded = $state(false);
	let error = $state<string | null>(null);
	let notice = $state<string | null>(null);
	let busy = $state(false);
	let newMembers = $state('');
	let editing = $state(false);
	let displayName = $state('');
	let parent = $state('');

	// Members, a page at a time.
	const memberPages = $state(cursorPages(50));
	let members = $state<MemberNode[]>([]);
	let memberTotal = $state(0);
	let memberQuery = $state('');
	let membersLoading = $state(true);
	let memberSeq = 0;

	async function load(): Promise<void> {
		try {
			team = (await graphql<TeamResult, Vars>(TEAM, { name })).team;
			error = null;
			if (team) await loadMembers();
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loaded = true;
		}
	}

	async function loadMembers(): Promise<void> {
		const seq = ++memberSeq;
		membersLoading = true;
		try {
			const result = await graphql<TeamMembersResult, Vars>(TEAM_MEMBERS, {
				name,
				first: memberPages.size,
				after: pageAfter(memberPages),
				search: memberQuery.trim() || null
			});
			if (seq !== memberSeq || !result.team) return;
			const conn = result.team.members;
			members = conn.nodes;
			memberTotal = conn.totalCount;
			pageLoaded(memberPages, conn.pageInfo);
			// The page emptied (members removed): step back to one with members.
			if (members.length === 0 && hasPreviousPage(memberPages)) {
				previousPage(memberPages);
				void loadMembers();
				return;
			}
		} catch (err) {
			if (seq === memberSeq) error = errorMessage(err);
		} finally {
			if (seq === memberSeq) membersLoading = false;
		}
	}

	function restartMembers(): void {
		firstPage(memberPages);
		void loadMembers();
	}

	let debounce: ReturnType<typeof setTimeout> | undefined;
	function onMemberInput(): void {
		clearTimeout(debounce);
		debounce = setTimeout(restartMembers, 200);
	}

	$effect(() => {
		void name;
		untrack(() => {
			clearTimeout(debounce);
			memberQuery = '';
			firstPage(memberPages);
			void load();
		});
		return () => clearTimeout(debounce);
	});

	async function run(action: () => Promise<unknown>, done?: string): Promise<void> {
		busy = true;
		error = null;
		notice = null;
		try {
			await action();
			if (done) notice = done;
			await load();
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = false;
		}
	}

	function addMembers(): Promise<void> {
		const usernames = parseUsernames(newMembers);
		if (!team || usernames.length === 0) return Promise.resolve();
		const id = team.id;
		return run(async () => {
			await graphql(ADD_TEAM_MEMBERS, { team: id, members: usernames.map((username) => ({ username })) });
			newMembers = '';
		});
	}

	function removeMember(member: MemberNode): Promise<void> {
		if (!team || !confirm(`Remove ${member.username} from ${team.name}?`)) return Promise.resolve();
		const id = team.id;
		return run(() => graphql(REMOVE_TEAM_MEMBERS, { team: id, members: [{ userID: member.id }] }));
	}

	function startEdit(): void {
		if (!team) return;
		displayName = team.displayName ?? '';
		parent = team.parentTeam?.name ?? '';
		editing = true;
	}

	function saveEdit(): Promise<void> {
		if (!team) return Promise.resolve();
		const t = team;
		const p = parent.trim();
		const parentChanged = p !== (t.parentTeam?.name ?? '');
		return run(async () => {
			await graphql(UPDATE_TEAM, {
				id: t.id,
				displayName: displayName.trim(),
				parentTeamName: parentChanged && p ? p : null,
				makeRoot: parentChanged && !p ? true : null
			});
			editing = false;
		}, 'Saved.');
	}

	async function remove(): Promise<void> {
		if (!team) return;
		const children = team.childTeams.totalCount;
		const prompt =
			children > 0
				? `Delete ${team.name} and its ${children} child team${children === 1 ? '' : 's'}?`
				: `Delete ${team.name}?`;
		if (!confirm(prompt)) return;
		busy = true;
		try {
			await graphql(DELETE_TEAM, { id: team.id });
			await goto(team.parentTeam ? teamUrl(team.parentTeam.name) : '/teams');
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = false;
		}
	}

	const canEdit = $derived(Boolean(team?.viewerCanAdminister));
</script>

<svelte:head><title>{name} · Teams · Sourcegull</title></svelte:head>

<div class="page">
	<nav class="crumbs" aria-label="Breadcrumb">
		<a href="/teams">Teams</a>
		{#if team?.parentTeam}
			/ <a href={teamUrl(team.parentTeam.name)}>{team.parentTeam.name}</a>
		{/if}
	</nav>

	{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}
	{#if notice}<div class="alert alert-success" role="status">{notice}</div>{/if}

	{#if loaded && !team && !error}
		<div class="empty-state">
			<h3>Team not found</h3>
			<p>There is no team named {name}.</p>
			<a class="btn" href="/teams">All teams</a>
		</div>
	{:else if team}
		<div class="page-header">
			<div class="title-block">
				<h1>
					{team.name}
					{#if team.syncedFromCodeHost}<span class="badge" title="Synced from a GitLab group">GitLab</span>{/if}
					{#if team.readonly}<span class="badge">Read-only</span>{/if}
				</h1>
				{#if team.displayName}<p class="lead">{team.displayName}</p>{/if}
				{#if team.syncedFromCodeHost}
					<p class="page-description">
						Mirrors a GitLab group: members and child teams change with the next sync from GitLab.
					</p>
				{:else if team.creator}
					<p class="page-description">Created by {team.creator.username}.</p>
				{/if}
			</div>
			{#if canEdit}
				<span class="page-actions">
					<button class="btn" type="button" disabled={busy} onclick={startEdit}>Edit</button>
					<button class="btn btn-danger" type="button" disabled={busy} onclick={remove}>Delete</button>
				</span>
			{/if}
		</div>

		{#if editing}
			<form
				class="card edit"
				onsubmit={(event) => {
					event.preventDefault();
					void saveEdit();
				}}
			>
				<div class="field">
					<label class="label" for="team-display">Display name</label>
					<input id="team-display" class="input" bind:value={displayName} />
				</div>
				<div class="field">
					<label class="label" for="team-parent">Parent team (empty for a top-level team)</label>
					<input id="team-parent" class="input" bind:value={parent} />
				</div>
				<div class="toolbar">
					<button class="btn btn-primary" type="submit" disabled={busy}>Save</button>
					<button class="btn" type="button" onclick={() => (editing = false)}>Cancel</button>
				</div>
			</form>
		{/if}

		<section>
			<h2>Members <span class="muted">({team.members.totalCount})</span></h2>
			{#if canEdit && !team.readonly}
				<form
					class="toolbar add"
					onsubmit={(event) => {
						event.preventDefault();
						void addMembers();
					}}
				>
					<label class="visually-hidden" for="new-members">Usernames to add</label>
					<input id="new-members" class="input" bind:value={newMembers} placeholder="Usernames, separated by commas" />
					<button class="btn btn-primary" type="submit" disabled={busy || !newMembers.trim()}>Add members</button>
				</form>
			{/if}
			<div class="list-bar">
				<label class="visually-hidden" for="member-filter">Filter members</label>
				<input
					id="member-filter"
					class="input filter"
					type="search"
					placeholder="Filter by username or display name…"
					bind:value={memberQuery}
					oninput={onMemberInput}
				/>
				<Pager
					pages={memberPages}
					shown={members.length}
					total={memberTotal}
					loading={membersLoading}
					onchange={loadMembers}
				/>
			</div>
			<div class="table-scroll">
				<table class="table table-bordered" aria-busy={membersLoading}>
					<thead>
						<tr>
							<th scope="col">Username</th>
							<th scope="col">Display name</th>
							{#if canEdit && !team.readonly}<th scope="col"><span class="visually-hidden">Actions</span></th>{/if}
						</tr>
					</thead>
					<tbody>
						{#each members as member (member.id)}
							<tr>
								<td><strong>{member.username}</strong></td>
								<td>{member.displayName ?? ''}</td>
								{#if canEdit && !team.readonly}
									<td class="actions">
										<button class="btn btn-sm" type="button" disabled={busy} onclick={() => removeMember(member)}
											>Remove</button
										>
									</td>
								{/if}
							</tr>
						{:else}
							<tr>
								<td colspan={canEdit && !team.readonly ? 3 : 2} class="empty-cell">
									{#if membersLoading}Loading…{:else if memberQuery.trim()}No members match.{:else}No members.{/if}
								</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		</section>

		<section>
			<h2>Child teams <span class="muted">({team.childTeams.totalCount})</span></h2>
			{#if team.childTeams.nodes.length === 0}
				<p class="muted">No child teams.</p>
			{:else}
				<ul class="members">
					{#each team.childTeams.nodes as child (child.id)}
						<li>
							<a href={teamUrl(child.name)}><strong>{child.name}</strong></a>
							{#if child.displayName}<span class="muted">{child.displayName}</span>{/if}
							<span class="muted">· {child.members.totalCount} members</span>
						</li>
					{/each}
				</ul>
			{/if}
		</section>
	{:else if !error}
		<p class="muted">Loading…</p>
	{/if}
</div>

<style>
	.title-block {
		min-width: 0;
	}
	h1 {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: var(--space-2);
		overflow-wrap: anywhere;
	}
	.lead {
		margin: var(--space-1) 0 0;
		font-size: var(--text-lg);
	}
	.edit {
		max-width: 32rem;
		margin-bottom: var(--space-4);
	}
	.add {
		flex-wrap: wrap;
		margin-bottom: var(--space-3);
	}
	.add .input {
		flex: 0 1 32rem;
		width: auto;
		min-width: 18rem;
	}
	.list-bar {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: var(--space-2) var(--space-4);
		margin-bottom: var(--space-3);
	}
	.list-bar .filter {
		flex: 0 1 20rem;
		width: auto;
		min-width: 12rem;
	}
	.actions {
		text-align: right;
		white-space: nowrap;
	}
	.members {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.members li {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		padding: var(--space-1) 0;
		border-bottom: 1px solid var(--color-border);
	}
	section {
		margin-top: var(--space-6);
	}
</style>
