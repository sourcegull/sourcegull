<script lang="ts">
	import { page } from '$app/state';
	import { untrack } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		ORGANIZATION,
		PENDING_INVITATIONS,
		RESEND_ORGANIZATION_INVITATION,
		REVOKE_ORGANIZATION_INVITATION,
		type InvitationNode,
		type OrganizationResult,
		type PendingInvitationsResult,
		type Vars
} from '#lib/orgs/queries.ts';

	const name = $derived(page.params.name ?? '');
	let orgId = $state<string | null>(null);
	let invitations = $state<InvitationNode[]>([]);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let notice = $state<string | null>(null);
	let busy = $state<string | null>(null);

	async function load(): Promise<void> {
		loading = true;
		try {
			const org = (await graphql<OrganizationResult, Vars>(ORGANIZATION, { name })).organization;
			orgId = org?.id ?? null;
			invitations = orgId
				? (await graphql<PendingInvitationsResult, Vars>(PENDING_INVITATIONS, { org: orgId })).pendingInvitations
				: [];
			error = null;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loading = false;
		}
	}

	$effect(() => {
		void name;
		untrack(() => void load());
	});

	async function act(inv: InvitationNode, action: 'revoke' | 'resend'): Promise<void> {
		if (action === 'revoke' && !confirm(`Revoke the invitation of ${recipient(inv)}?`)) return;
		busy = inv.id;
		error = null;
		notice = null;
		try {
			await graphql(action === 'revoke' ? REVOKE_ORGANIZATION_INVITATION : RESEND_ORGANIZATION_INVITATION, {
				id: inv.id
			});
			notice = action === 'revoke' ? 'Invitation revoked.' : `Invitation emailed again to ${recipient(inv)}.`;
			await load();
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = null;
		}
	}

	const recipient = (inv: InvitationNode) => inv.recipient?.username ?? inv.recipientEmail ?? 'unknown';
</script>

<svelte:head><title>Pending invitations · {name} · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Pending invitations</h1>
		<p class="page-description">Invite people from the <a href="/organizations/{name}/members">members</a> page.</p>
	</div>
</div>

{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}
{#if notice}<div class="alert alert-success" role="status">{notice}</div>{/if}

<div class="table-scroll">
	<table class="table" aria-busy={loading}>
		<thead>
			<tr>
				<th scope="col">Recipient</th>
				<th scope="col">Invited by</th>
				<th scope="col">Sent</th>
				<th scope="col">Expires</th>
				<th scope="col"><span class="visually-hidden">Actions</span></th>
			</tr>
		</thead>
		<tbody>
			{#each invitations as inv (inv.id)}
				<tr>
					<td><strong>{recipient(inv)}</strong></td>
					<td>{inv.sender?.username ?? ''}</td>
					<td>
						{new Date(inv.createdAt).toLocaleString()}
						{#if inv.notifiedAt}<span class="badge" title="Emailed {new Date(inv.notifiedAt).toLocaleString()}"
								>Emailed</span
							>{/if}
					</td>
					<td>{inv.expiresAt ? new Date(inv.expiresAt).toLocaleString() : 'Never'}</td>
					<td class="actions">
						<button class="btn btn-sm" type="button" disabled={busy !== null} onclick={() => act(inv, 'resend')}
							>Resend</button
						>
						<button class="btn btn-sm btn-danger" type="button" disabled={busy !== null} onclick={() => act(inv, 'revoke')}
							>Revoke</button
						>
					</td>
				</tr>
			{:else}
				{#if !loading}<tr><td colspan="5" class="muted">No pending invitations.</td></tr>{/if}
			{/each}
		</tbody>
	</table>
</div>

<style>
	.actions {
		text-align: right;
		white-space: nowrap;
	}
</style>
