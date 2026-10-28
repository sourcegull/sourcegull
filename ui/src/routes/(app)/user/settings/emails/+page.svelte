<script lang="ts">
	import { onMount } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		ACCOUNT_EMAILS,
		ADD_USER_EMAIL,
		REMOVE_USER_EMAIL,
		RESEND_VERIFICATION_EMAIL,
		SET_USER_EMAIL_PRIMARY,
		type AccountEmailsResult
	} from '#lib/account/queries.ts';

	let user = $state<AccountEmailsResult['currentUser']>(null);
	let newEmail = $state('');
	let error = $state<string | null>(null);
	let notice = $state<string | null>(null);
	let busy = $state(false);

	async function load(): Promise<void> {
		try {
			user = (await graphql<AccountEmailsResult>(ACCOUNT_EMAILS)).currentUser;
		} catch (err) {
			error = errorMessage(err);
		}
	}

	async function run(mutation: string, email: string, done: string): Promise<void> {
		if (!user) return;
		busy = true;
		error = null;
		notice = null;
		try {
			await graphql<unknown, Record<string, unknown>>(mutation, { user: user.id, email });
			notice = done;
			await load();
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = false;
		}
	}

	async function add(event: SubmitEvent): Promise<void> {
		event.preventDefault();
		const email = newEmail.trim();
		await run(ADD_USER_EMAIL, email, `Added ${email}. Open the link sent to it to verify it.`);
		if (!error) newEmail = '';
	}

	onMount(() => {
		void load();
	});
</script>

<svelte:head><title>Emails · Settings · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Emails</h1>
		<p class="page-description">
			Verified addresses sign you in and match you to code host accounts and permissions. The
			primary address receives notifications.
		</p>
	</div>
</div>

{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}
{#if notice}<div class="alert alert-success" role="status">{notice}</div>{/if}
{#if user?.scimControlled}
	<div class="alert alert-info" role="note">Your identity provider manages your email addresses.</div>
{/if}

{#if user}
	<ul class="list-group">
		{#each user.emails as e (e.email)}
			<li class="row">
				<strong>{e.email}</strong>
				{#if e.isPrimary}<span class="badge badge-primary">Primary</span>{/if}
				{#if e.verified}<span class="badge">Verified</span>{:else}<span class="badge badge-warning">Not verified</span>{/if}
				{#if !user.scimControlled}
					<span class="toolbar">
						{#if !e.verified}
							<button class="btn btn-sm" type="button" disabled={busy}
								onclick={() => run(RESEND_VERIFICATION_EMAIL, e.email, `Sent a verification link to ${e.email}.`)}
								>Resend verification</button
							>
						{/if}
						{#if e.verified && !e.isPrimary}
							<button class="btn btn-sm" type="button" disabled={busy}
								onclick={() => run(SET_USER_EMAIL_PRIMARY, e.email, `${e.email} is now your primary address.`)}
								>Set as primary</button
							>
						{/if}
						{#if !e.isPrimary && user.emails.length > 1}
							<button class="btn btn-sm btn-danger" type="button" disabled={busy}
								onclick={() => run(REMOVE_USER_EMAIL, e.email, `Removed ${e.email}.`)}>Remove</button
							>
						{/if}
					</span>
				{/if}
			</li>
		{/each}
	</ul>

	{#if !user.scimControlled}
		<form class="add" onsubmit={add}>
			<label class="visually-hidden" for="new-email">Email address</label>
			<input id="new-email" class="input" type="email" bind:value={newEmail} placeholder="you@example.com" required />
			<button class="btn btn-primary" type="submit" disabled={busy}>Add email</button>
		</form>
	{/if}
{:else if !error}
	<p class="muted">Loading…</p>
{/if}

<style>
	.row {
		display: flex !important;
		align-items: center;
		gap: var(--space-2);
		flex-wrap: wrap;
	}
	.row .toolbar {
		margin-left: auto;
	}
	.add {
		display: flex;
		gap: var(--space-2);
		margin-top: var(--space-4);
		max-width: 32rem;
	}
	.add .input {
		flex: 1;
	}
</style>
