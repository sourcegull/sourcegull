<script lang="ts">
	import Icon from '#lib/components/Icon.svelte';
	import { onMount } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		RESPOND_TO_ORGANIZATION_INVITATION,
		VIEWER_ORGANIZATIONS,
		type InvitationResponse,
		type ViewerInvitation,
		type ViewerOrganizationsResult
	} from '#lib/orgs/queries.ts';

	let result = $state<ViewerOrganizationsResult | null>(null);
	let error = $state<string | null>(null);
	let busy = $state<string | null>(null);

	async function load(): Promise<void> {
		try {
			result = await graphql<ViewerOrganizationsResult>(VIEWER_ORGANIZATIONS);
			error = null;
		} catch (err) {
			error = errorMessage(err);
		}
	}

	async function respond(inv: ViewerInvitation, response: InvitationResponse): Promise<void> {
		busy = inv.id;
		error = null;
		try {
			await graphql(RESPOND_TO_ORGANIZATION_INVITATION, { id: inv.id, response });
			await load();
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = null;
		}
	}

	onMount(() => {
		void load();
	});
</script>

<svelte:head><title>Organizations · Settings · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Organizations</h1>
		<p class="page-description">Organizations share settings, saved searches, notebooks and bulk changes among members.</p>
	</div>
	<div class="page-actions">
		<a class="btn btn-primary" href="/organizations/new">New organization</a>
	</div>
</div>
{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}

{#if result && result.viewerOrganizationInvitations.length > 0}
	<section>
		<h2>Invitations</h2>
		<ul class="list">
			{#each result.viewerOrganizationInvitations as inv (inv.id)}
				<li class="card row">
					<span>
						<strong>{inv.organization.displayName ?? inv.organization.name}</strong>
						<span class="muted">from {inv.sender?.username ?? 'someone'}</span>
					</span>
					<span class="toolbar">
						<button class="btn btn-sm btn-primary" type="button" disabled={busy !== null} onclick={() => respond(inv, 'ACCEPT')}
							>Join</button
						>
						<button class="btn btn-sm" type="button" disabled={busy !== null} onclick={() => respond(inv, 'REJECT')}
							>Decline</button
						>
					</span>
				</li>
			{/each}
		</ul>
	</section>
{/if}

<section>
	<h2>Your organizations</h2>
	{#if !result}
		{#if !error}<p class="muted">Loading…</p>{/if}
	{:else if result.currentUser?.organizations.nodes.length}
		<ul class="list list-group">
			{#each result.currentUser.organizations.nodes as org (org.id)}
				<li class="row">
					<a href={org.url}><strong>{org.name}</strong></a>
					<span class="muted">{org.displayName ?? ''}</span>
					<span class="toolbar">
						<a class="btn btn-sm" href="{org.url}/members">Members</a>
						<a class="btn btn-sm" href="{org.url}/settings">Settings</a>
					</span>
				</li>
			{/each}
		</ul>
	{:else}
		<div class="empty-state">
			<span class="icon"><Icon name="org" size={32} /></span>
			<h3>No organizations</h3>
			<p>You are not a member of any organization. Create one, or ask a member to invite you.</p>
			<a class="btn btn-primary" href="/organizations/new">New organization</a>
		</div>
	{/if}
</section>

<style>
	.list:not(.list-group) {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: var(--space-2);
	}
	.row {
		display: flex !important;
		align-items: center;
		gap: var(--space-3);
		flex-wrap: wrap;
	}
	.row .toolbar {
		margin-left: auto;
	}
	section {
		margin-bottom: var(--space-6);
	}
</style>
