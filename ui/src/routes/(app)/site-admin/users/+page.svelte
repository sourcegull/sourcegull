<script lang="ts">
	import { onMount } from 'svelte';
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { resetAuthState } from '#lib/auth.ts';
	import ManagedNotice from '#lib/components/ManagedNotice.svelte';
	import { isExclusive } from '#lib/declared-config.ts';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		SET_USER_IS_SITE_ADMIN,
		USERS,
		RBAC,
		SET_ROLES,
		type RbacResult,
		type RoleSummary,
		type SetUserIsSiteAdminVariables,
		type UserNode,
		type UsersResult,
		type UsersVariables
	} from '#lib/queries.ts';
	import { DEACTIVATE_USER, REACTIVATE_USER } from '#lib/account/queries.ts';
	import type { PageProps } from './$types';

	let { data }: PageProps = $props();

	const PAGE_SIZE = 50;

	let users = $state<UserNode[]>([]);
	let totalCount = $state(0);
	// `?query=` prefills the filter, for links to one user.
	let query = $state(page.url.searchParams.get('query') ?? '');
	let includeDeactivated = $state(false);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let pending = $state<string | null>(null);
	let requestSeq = 0;
	let availableRoles = $state<RoleSummary[]>([]);
	let editing = $state<string | null>(null);
	let selectedRoles = $state<string[]>([]);
	/** Whether the resources file decides every site admin. */
	let siteAdminsExclusive = $state(false);

	async function loadRoles(): Promise<void> {
		try {
			const r = await graphql<RbacResult>(RBAC);
			availableRoles = r.roles.nodes;
			siteAdminsExclusive = isExclusive(r.site.declaredConfigFiles, 'siteAdmins');
		} catch (err) {
			error = errorMessage(err);
		}
	}

	function editRoles(user: UserNode): void {
		editing = user.id;
		selectedRoles = user.roles.nodes.filter((r) => !r.system).map((r) => r.id);
	}

	function toggleRole(id: string): void {
		selectedRoles = selectedRoles.includes(id)
			? selectedRoles.filter((r) => r !== id)
			: [...selectedRoles, id];
	}

	async function saveRoles(user: UserNode): Promise<void> {
		pending = user.id;
		try {
			const adminRole = availableRoles.find((r) => r.name === 'SITE_ADMINISTRATOR');
			if (!adminRole)
				throw new Error('Could not load system roles. Reload this page and try again.');
			await graphql(SET_ROLES, {
				user: user.id,
				roles: [...selectedRoles, ...(user.siteAdmin ? [adminRole.id] : [])]
			});
			editing = null;
			await load();
		} catch (err) {
			error = errorMessage(err);
		} finally {
			pending = null;
		}
	}

	async function load(): Promise<void> {
		const seq = ++requestSeq;
		loading = true;
		error = null;
		try {
			const result = await graphql<UsersResult, UsersVariables>(USERS, {
				first: PAGE_SIZE,
				query: query.trim() || null,
				includeDeactivated
			});
			if (seq !== requestSeq) return;
			users = result.users.nodes;
			totalCount = result.users.totalCount;
		} catch (err) {
			if (seq === requestSeq) error = errorMessage(err);
		} finally {
			if (seq === requestSeq) loading = false;
		}
	}

	let debounce: ReturnType<typeof setTimeout> | undefined;
	function oninput(): void {
		clearTimeout(debounce);
		debounce = setTimeout(load, 200);
	}

	async function toggleSiteAdmin(user: UserNode): Promise<void> {
		const siteAdmin = !user.siteAdmin;
		if (
			!siteAdmin &&
			user.id === data.auth.user?.id &&
			!confirm('Remove your own site admin access? You will lose access to this page.')
		) {
			return;
		}
		pending = user.id;
		error = null;
		try {
			await graphql<unknown, SetUserIsSiteAdminVariables>(SET_USER_IS_SITE_ADMIN, {
				userID: user.id,
				siteAdmin
			});
			if (!siteAdmin && user.id === data.auth.user?.id) {
				resetAuthState();
				await goto('/search', { invalidateAll: true });
				return;
			}
			await load();
		} catch (err) {
			error = `Could not update ${user.username}: ${errorMessage(err)}`;
		} finally {
			pending = null;
		}
	}

	async function setActive(user: UserNode, active: boolean): Promise<void> {
		if (
			!active &&
			!confirm(
				`Deactivate ${user.username}? They are signed out, their access tokens are revoked and they cannot sign in until reactivated.`
			)
		) {
			return;
		}
		pending = user.id;
		error = null;
		try {
			await graphql<unknown, { user: string }>(active ? REACTIVATE_USER : DEACTIVATE_USER, {
				user: user.id
			});
			await load();
		} catch (err) {
			error = `Could not update ${user.username}: ${errorMessage(err)}`;
		} finally {
			pending = null;
		}
	}

	const formatDate = (iso: string) => new Date(iso).toLocaleDateString();

	onMount(() => {
		void load();
		void loadRoles();
		return () => clearTimeout(debounce);
	});
</script>

<svelte:head><title>Users · Site admin · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Users</h1>
		<p class="page-description">
			{totalCount} total. Promote site admins and assign roles; roles grant permissions on the
			<a href="/site-admin/roles">roles</a> page.
		</p>
	</div>
</div>

<div class="field filter">
	<label class="visually-hidden" for="user-filter">Filter users</label>
	<input
		id="user-filter"
		class="input"
		type="search"
		placeholder="Filter by username or name…"
		bind:value={query}
		{oninput}
	/>
	<label class="checkbox"
		><input type="checkbox" bind:checked={includeDeactivated} onchange={load} /> Show deactivated users</label
	>
</div>

{#if siteAdminsExclusive}<ManagedNotice kind="siteAdmins" />{/if}

{#if error}
	<div class="alert alert-danger" role="alert">{error}</div>
{/if}

<div class="table-scroll">
	<table class="table table-bordered" aria-busy={loading}>
		<thead>
			<tr>
				<th scope="col">Username</th>
				<th scope="col">Display name</th>
				<th scope="col">Created</th>
				<th scope="col">Role</th>
				<th scope="col"><span class="visually-hidden">Actions</span></th>
			</tr>
		</thead>
		<tbody>
			{#each users as user (user.id)}
				<tr>
					<td>
						<strong>{user.username}</strong>
						{#if user.id === data.auth.user?.id}<span class="muted">(you)</span>{/if}
						{#if user.deactivatedAt}<span
								class="badge badge-warning"
								title="Deactivated {formatDate(user.deactivatedAt)}">Deactivated</span
							>{/if}
						{#if user.scimControlled}<span
								class="badge"
								title="Managed by your identity provider through SCIM; change it there">SCIM</span
							>{/if}
					</td>
					<td>{user.displayName ?? ''}</td>
					<td>{formatDate(user.createdAt)}</td>
					<td>
						{#if user.siteAdmin}<span class="badge badge-primary">Site admin</span>{:else}<span
								class="badge">User</span
							>{/if}
						{#each user.roles.nodes.filter((role) => !role.system) as role (role.id)}<span
								class="badge">{role.name}</span
							>{/each}
					</td>
					<td class="actions">
						{#if user.deactivatedAt}
							{#if !user.scimControlled}
								<button
									class="btn btn-sm"
									type="button"
									disabled={pending === user.id}
									onclick={() => setActive(user, true)}>Reactivate</button
								>
							{/if}
						{:else}
						{#if !siteAdminsExclusive}
							<button
								class="btn btn-sm"
								type="button"
								disabled={pending === user.id}
								onclick={() => toggleSiteAdmin(user)}
							>
								{user.siteAdmin ? 'Revoke site admin' : 'Promote to site admin'}
							</button>
						{/if}
						<button class="btn btn-sm" type="button" onclick={() => editRoles(user)}
							>Edit roles</button
						>
						{#if !user.scimControlled && user.id !== data.auth.user?.id}
							<button
								class="btn btn-sm btn-danger"
								type="button"
								disabled={pending === user.id}
								onclick={() => setActive(user, false)}>Deactivate</button
							>
						{/if}
						{/if}
					</td>
				</tr>
				{#if editing === user.id}
					<tr
						><td colspan="5">
							<div class="role-editor">
								<strong>Roles for {user.username}</strong>
								{#each availableRoles.filter((role) => !role.system) as role (role.id)}
									<label
										><input
											type="checkbox"
											checked={selectedRoles.includes(role.id)}
											onchange={() => toggleRole(role.id)}
										/>
										{role.name}</label
									>
								{/each}
								<button
									class="btn btn-sm btn-primary"
									type="button"
									disabled={pending === user.id}
									onclick={() => saveRoles(user)}>Save roles</button
								>
								<button class="btn btn-sm" type="button" onclick={() => (editing = null)}
									>Cancel</button
								>
							</div>
						</td></tr
					>
				{/if}
			{:else}
				<tr>
					<td colspan="5" class="empty-cell">{loading ? 'Loading…' : 'No users found.'}</td>
				</tr>
			{/each}
		</tbody>
	</table>
</div>

{#if totalCount > users.length}
	<p class="muted">
		Showing the first {users.length} of {totalCount}. Use the filter to narrow the list.
	</p>
{/if}

<style>
	.filter {
		max-width: 24rem;
	}
	.checkbox {
		display: inline-flex;
		align-items: center;
		gap: var(--space-1);
		margin-top: var(--space-2);
	}
	.actions {
		text-align: right;
		white-space: nowrap;
	}
	td :global(.badge + .badge) {
		margin-left: var(--space-1);
	}
	.role-editor {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: var(--space-3);
	}
</style>
