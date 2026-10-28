<script lang="ts">
	import { page } from '$app/state';
	import SidebarLayout from '#lib/components/SidebarLayout.svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		ORGANIZATION,
		RESPOND_TO_ORGANIZATION_INVITATION,
		type InvitationResponse,
		type OrganizationResult,
		type OrgHeader,
		type Vars
} from '#lib/orgs/queries.ts';
	import type { LayoutProps } from './$types';

	let { children }: LayoutProps = $props();

	const name = $derived(page.params.name ?? '');
	let org = $state<OrgHeader | null>(null);
	let loaded = $state(false);
	let error = $state<string | null>(null);
	let responding = $state(false);

	async function load(orgName: string): Promise<void> {
		try {
			org = (await graphql<OrganizationResult, Vars>(ORGANIZATION, { name: orgName })).organization;
			error = null;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loaded = true;
		}
	}

	$effect(() => {
		void load(name);
	});

	async function respond(response: InvitationResponse): Promise<void> {
		if (!org?.viewerPendingInvitation) return;
		responding = true;
		try {
			await graphql(RESPOND_TO_ORGANIZATION_INVITATION, { id: org.viewerPendingInvitation.id, response });
			// Members see more pages; reload everything.
			location.reload();
		} catch (err) {
			error = errorMessage(err);
		} finally {
			responding = false;
		}
	}

	const base = $derived(`/organizations/${encodeURIComponent(name)}`);
	const items = $derived([
		{ href: `${base}/members`, title: 'Members' },
		...(org?.viewerCanAdminister
			? [
					{ href: `${base}/invitations`, title: 'Pending invitations' },
					{ href: `${base}/settings`, title: 'Settings' }
				]
			: []),
		{ href: `${base}/bulk-changes`, title: 'Bulk changes' }
	]);
</script>

{#if loaded && !org && !error}
	<div class="missing">
		<h1>Organization not found</h1>
		<p class="muted">There is no organization named {name}.</p>
	</div>
{:else}
	<SidebarLayout title={org?.displayName ?? name} {items}>
		{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}
		{#if org?.viewerPendingInvitation}
			<div class="alert alert-info invitation" role="status">
				<span
					>{org.viewerPendingInvitation.sender?.username ?? 'Someone'} invited you to join
					<strong>{org.displayName ?? org.name}</strong>.</span
				>
				<span class="toolbar">
					<button class="btn btn-sm btn-primary" type="button" disabled={responding} onclick={() => respond('ACCEPT')}
						>Join organization</button
					>
					<button class="btn btn-sm" type="button" disabled={responding} onclick={() => respond('REJECT')}
						>Decline</button
					>
				</span>
			</div>
		{/if}
		{@render children()}
	</SidebarLayout>
{/if}

<style>
	.missing {
		max-width: 48rem;
		margin: 0 auto;
		padding: var(--space-6) var(--space-4);
	}
	.invitation {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--space-3);
		flex-wrap: wrap;
	}
</style>
