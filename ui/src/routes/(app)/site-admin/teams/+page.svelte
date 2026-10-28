<script lang="ts">
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import { SYNC_TEAMS_FROM_GITLAB_GROUPS, type SyncTeamsResult } from '#lib/orgs/queries.ts';

	let busy = $state(false);
	let error = $state<string | null>(null);
	let result = $state<SyncTeamsResult['syncTeamsFromGitLabGroups'] | null>(null);

	async function sync(): Promise<void> {
		busy = true;
		error = null;
		try {
			result = (await graphql<SyncTeamsResult>(SYNC_TEAMS_FROM_GITLAB_GROUPS)).syncTeamsFromGitLabGroups;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = false;
		}
	}
</script>

<svelte:head><title>Teams · Site admin · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Teams</h1>
		<p class="page-description">Teams group users for code ownership. They can be synced from GitLab groups.</p>
	</div>
	<div class="page-actions">
		<a class="btn" href="/teams">Browse teams</a>
	</div>
</div>

<section class="card">
	<div class="card-header"><h2>Sync from GitLab groups</h2></div>
	<p>
		With <code>"own.syncTeamsFromGitLabGroups": true</code> in the
		<a href="/site-admin/configuration">site configuration</a>, the worker turns every GitLab group
		that a code host connection's token can see into a read-only team each hour: nested groups become
		child teams, and group members with a linked GitLab account become team members. Groups that
		disappear delete their teams.
	</p>
	<button class="btn btn-primary" type="button" onclick={sync} disabled={busy}
		>{busy ? 'Syncing…' : 'Sync now'}</button
	>
	{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}
	{#if result}
		<div class="alert alert-success" role="status">
			Created {result.created}, updated {result.updated}, deleted {result.deleted} teams; skipped {result.skipped}
			groups; {result.members} memberships.
		</div>
		{#if result.warnings.length > 0}
			<div class="alert alert-warning" role="status">
				<ul>
					{#each result.warnings as warning, i (i)}<li>{warning}</li>{/each}
				</ul>
			</div>
		{/if}
	{/if}
</section>

<style>
	.card {
		max-width: 48rem;
	}
	.card .alert {
		margin-top: var(--space-4);
	}
</style>
