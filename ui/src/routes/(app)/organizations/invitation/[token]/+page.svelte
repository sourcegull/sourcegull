<script lang="ts">
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { untrack } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		INVITATION_BY_TOKEN,
		RESPOND_TO_ORGANIZATION_INVITATION,
		type InvitationByTokenResult,
		type InvitationResponse,
		type Vars
} from '#lib/orgs/queries.ts';

	const token = $derived(page.params.token ?? '');
	let invitation = $state<InvitationByTokenResult['invitationByToken'] | null>(null);
	let error = $state<string | null>(null);
	let loading = $state(true);
	let busy = $state(false);
	let declined = $state(false);

	async function load(): Promise<void> {
		loading = true;
		try {
			invitation = (await graphql<InvitationByTokenResult, Vars>(INVITATION_BY_TOKEN, { token })).invitationByToken;
			error = null;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loading = false;
		}
	}

	$effect(() => {
		void token;
		untrack(() => void load());
	});

	async function respond(response: InvitationResponse): Promise<void> {
		if (!invitation) return;
		busy = true;
		error = null;
		try {
			await graphql(RESPOND_TO_ORGANIZATION_INVITATION, { id: invitation.id, response });
			if (response === 'ACCEPT') await goto(invitation.organization.url);
			else declined = true;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = false;
		}
	}

	const answered = $derived(Boolean(invitation?.respondedAt || invitation?.revokedAt));
	const expired = $derived(Boolean(invitation?.expiresAt && Date.parse(invitation.expiresAt) < Date.now()));
</script>

<svelte:head><title>Organization invitation · Sourcegull</title></svelte:head>

<div class="invitation card">
	{#if loading}
		<p class="muted">Loading…</p>
	{:else if invitation}
		{@const org = invitation.organization}
		<h1>Join {org.displayName ?? org.name}</h1>
		<p>
			{invitation.sender?.username ?? 'Someone'} invited you to join the organization
			<strong>{org.name}</strong>.
		</p>
		{#if invitation.recipientEmail && invitation.isVerifiedEmail === false}
			<div class="alert alert-warning" role="status">
				This invitation was sent to {invitation.recipientEmail}. Add and verify that address in your
				account to accept it.
			</div>
		{/if}
		{#if declined}
			<div class="alert alert-info" role="status">You declined the invitation.</div>
		{:else if invitation.revokedAt}
			<div class="alert alert-warning" role="status">The invitation was revoked.</div>
		{:else if invitation.respondedAt}
			<div class="alert alert-info" role="status">The invitation was already answered.</div>
		{:else if expired}
			<div class="alert alert-warning" role="status">The invitation has expired; ask for a new one.</div>
		{/if}
		{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}
		{#if !answered && !expired && !declined}
			<div class="toolbar">
				<button class="btn btn-primary" type="button" disabled={busy} onclick={() => respond('ACCEPT')}
					>Join {org.name}</button
				>
				<button class="btn" type="button" disabled={busy} onclick={() => respond('REJECT')}>Decline</button>
			</div>
		{/if}
	{:else}
		<h1>Invitation not found</h1>
		<p class="muted">The link is invalid, or the invitation is for another user. {error ?? ''}</p>
	{/if}
</div>

<style>
	.invitation {
		max-width: 36rem;
		margin: var(--space-8) auto;
	}
</style>
