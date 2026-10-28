<script lang="ts">
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { untrack } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		ADD_USER_TO_ORGANIZATION,
		INVITE_USER_TO_ORGANIZATION,
		ORGANIZATION,
		ORGANIZATION_MEMBERS,
		REMOVE_USER_FROM_ORGANIZATION,
		isEmailAddress,
		type InviteUserToOrganizationResult,
		type MemberNode,
		type OrganizationMembersResult,
		type OrganizationResult,
		type OrgHeader,
		type Vars
} from '#lib/orgs/queries.ts';
	import type { PageProps } from './$types';

	let { data }: PageProps = $props();

	const PAGE_SIZE = 50;
	const name = $derived(page.params.name ?? '');

	let org = $state<OrgHeader | null>(null);
	let members = $state<MemberNode[]>([]);
	let totalCount = $state(0);
	let endCursor = $state<string | null>(null);
	let query = $state('');
	let loading = $state(true);
	let error = $state<string | null>(null);
	let busy = $state<string | null>(null);
	let invitee = $state('');
	let invited = $state<{ who: string; url: string; emailed: boolean } | null>(null);
	let requestSeq = 0;

	async function load(more = false): Promise<void> {
		const seq = ++requestSeq;
		loading = true;
		try {
			if (!more) {
				org = (await graphql<OrganizationResult, Vars>(ORGANIZATION, { name })).organization;
				if (!org?.viewerIsMember && !org?.viewerCanAdminister) {
					members = [];
					return;
				}
			}
			const result = await graphql<OrganizationMembersResult, Vars>(ORGANIZATION_MEMBERS, {
				name,
				first: PAGE_SIZE,
				after: more ? endCursor : null,
				query: query.trim() || null
			});
			if (seq !== requestSeq || !result.organization) return;
			const conn = result.organization.members;
			members = more ? [...members, ...conn.nodes] : conn.nodes;
			totalCount = conn.totalCount;
			endCursor = conn.pageInfo.hasNextPage ? conn.pageInfo.endCursor : null;
			error = null;
		} catch (err) {
			if (seq === requestSeq) error = errorMessage(err);
		} finally {
			if (seq === requestSeq) loading = false;
		}
	}

	$effect(() => {
		void name;
		untrack(() => void load());
	});

	let debounce: ReturnType<typeof setTimeout> | undefined;
	function oninput(): void {
		clearTimeout(debounce);
		debounce = setTimeout(() => load(), 200);
	}

	async function invite(): Promise<void> {
		if (!org) return;
		const who = invitee.trim();
		busy = 'invite';
		error = null;
		invited = null;
		try {
			const vars = isEmailAddress(who)
				? { organization: org.id, email: who }
				: { organization: org.id, username: who.replace(/^@/, '') };
			const result = await graphql<InviteUserToOrganizationResult, Vars>(INVITE_USER_TO_ORGANIZATION, vars);
			invited = {
				who,
				url: result.inviteUserToOrganization.invitationURL,
				emailed: result.inviteUserToOrganization.sentInvitationEmail
			};
			invitee = '';
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = null;
		}
	}

	async function addDirectly(): Promise<void> {
		if (!org) return;
		busy = 'add';
		error = null;
		invited = null;
		try {
			await graphql(ADD_USER_TO_ORGANIZATION, { organization: org.id, username: invitee.trim().replace(/^@/, '') });
			invitee = '';
			await load();
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = null;
		}
	}

	async function remove(member: MemberNode): Promise<void> {
		if (!org) return;
		const self = member.id === data.auth.user?.id;
		const prompt = self
			? `Leave ${org.name}? You need a new invitation to join again.`
			: `Remove ${member.username} from ${org.name}?`;
		if (!confirm(prompt)) return;
		busy = member.id;
		error = null;
		try {
			await graphql(REMOVE_USER_FROM_ORGANIZATION, { user: member.id, organization: org.id });
			if (self && !data.auth.user?.siteAdmin) {
				await goto('/user/settings/organizations');
				return;
			}
			await load();
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = null;
		}
	}
</script>

<svelte:head><title>Members · {name} · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Members</h1>
		{#if org?.viewerCanAdminister}<p class="page-description">{totalCount} total</p>{/if}
	</div>
</div>

{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}

{#if org && !org.viewerCanAdminister}
	<p class="muted">Only members of {org.displayName ?? org.name} and site admins can see its members.</p>
{:else if org}
	<form
		class="toolbar invite-row"
		onsubmit={(event) => {
			event.preventDefault();
			void invite();
		}}
	>
		<label class="visually-hidden" for="invitee">Username or email address</label>
		<input id="invitee" class="input" placeholder="Username or email address" bind:value={invitee} required />
		<button class="btn btn-primary" type="submit" disabled={busy !== null || !invitee.trim()}>Invite</button>
		{#if data.auth.user?.siteAdmin}
			<button
				class="btn"
				type="button"
				title="Site admins can add a user without an invitation"
				disabled={busy !== null || !invitee.trim() || isEmailAddress(invitee)}
				onclick={addDirectly}>Add directly</button
			>
		{/if}
	</form>
	{#if invited}
		<div class="alert alert-success" role="status">
			Invited {invited.who}.
			{invited.emailed ? 'An email with this link was sent:' : 'Email is not configured; send them this link:'}
			<code class="link">{invited.url}</code>
			<button class="btn btn-sm" type="button" onclick={() => navigator.clipboard?.writeText(invited?.url ?? '')}
				>Copy link</button
			>
			The invitation expires in 48 hours.
		</div>
	{/if}

	<div class="field">
		<label class="visually-hidden" for="member-filter">Filter members</label>
		<input
			id="member-filter"
			class="input"
			type="search"
			placeholder="Filter by username or name…"
			bind:value={query}
			{oninput}
		/>
	</div>

	<div class="table-scroll">
		<table class="table" aria-busy={loading}>
			<thead>
				<tr>
					<th scope="col">Username</th>
					<th scope="col">Display name</th>
					<th scope="col"><span class="visually-hidden">Actions</span></th>
				</tr>
			</thead>
			<tbody>
				{#each members as member (member.id)}
					<tr>
						<td>
							<strong>{member.username}</strong>
							{#if member.id === data.auth.user?.id}<span class="muted">(you)</span>{/if}
						</td>
						<td>{member.displayName ?? ''}</td>
						<td class="actions">
							<button class="btn btn-sm" type="button" disabled={busy !== null} onclick={() => remove(member)}
								>{member.id === data.auth.user?.id ? 'Leave' : 'Remove'}</button
							>
						</td>
					</tr>
				{:else}
					{#if !loading}<tr><td colspan="3" class="muted">No members match.</td></tr>{/if}
				{/each}
			</tbody>
		</table>
	</div>
	{#if endCursor}
		<button class="btn" type="button" disabled={loading} onclick={() => load(true)}>Show more</button>
	{/if}
{:else if loading}
	<p class="muted">Loading…</p>
{/if}

<style>
	.invite-row {
		flex-wrap: wrap;
		margin-bottom: var(--space-3);
	}
	.link {
		word-break: break-all;
	}
	.actions {
		text-align: right;
	}
</style>
