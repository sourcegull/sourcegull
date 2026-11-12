<script lang="ts">
	import { formatDate } from '#lib/format.svelte.ts';
	import CountTile from '#lib/components/CountTile.svelte';
	import Pager from '#lib/components/Pager.svelte';
	import RelatedLinks from '#lib/components/RelatedLinks.svelte';
	import SortHeader from '#lib/components/SortHeader.svelte';
	import { cursorPages, firstPage, hasPreviousPage, pageAfter, pageLoaded, previousPage } from '#lib/cursor-pages.ts';
	import { onMount } from 'svelte';
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { EMAIL_SETTINGS_URL, resetAuthState } from '#lib/auth.ts';
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
	import type { CreateUserMutation, CreateUserMutationVariables, UserOrderBy } from '#lib/graphql-types.ts';
	import {
		CREATE_USER,
		DEACTIVATE_USER,
		RANDOMIZE_USER_PASSWORD,
		REACTIVATE_USER,
		type RandomizeUserPasswordResult
	} from '#lib/account/queries.ts';
	import type { PageProps } from './$types';

	let { data }: PageProps = $props();

	const pages = $state(cursorPages(50));
	let users = $state<UserNode[]>([]);
	let totalCount = $state(0);
	let counts = $state<UsersResult['userCounts'] | null>(null);
	let order = $state<UserOrderBy>('USERNAME');
	let descending = $state(false);
	// `?query=` prefills the filter, for links to one user.
	let query = $state(page.url.searchParams.get('query') ?? '');
	let includeDeactivated = $state(false);
	/** Which users the list shows: all of them, only site admins, or only deactivated users. */
	let show = $state<'all' | 'admins' | 'deactivated'>('all');
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
		selectedRoles = selectedRoles.includes(id) ? selectedRoles.filter((r) => r !== id) : [...selectedRoles, id];
	}

	async function saveRoles(user: UserNode): Promise<void> {
		pending = user.id;
		try {
			const adminRole = availableRoles.find((r) => r.name === 'SITE_ADMINISTRATOR');
			if (!adminRole) throw new Error('Could not load system roles. Reload this page and try again.');
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
				first: pages.size,
				after: pageAfter(pages),
				query: query.trim() || null,
				includeDeactivated,
				deactivated: show === 'deactivated',
				siteAdmin: show === 'admins' ? true : null,
				orderBy: order,
				descending
			});
			if (seq !== requestSeq) return;
			users = result.users.nodes;
			totalCount = result.users.totalCount;
			counts = result.userCounts;
			pageLoaded(pages, result.users.pageInfo);
			// The page emptied (users deactivated, or fewer matches): step back to one with users.
			if (users.length === 0 && hasPreviousPage(pages)) {
				previousPage(pages);
				void load();
				return;
			}
		} catch (err) {
			if (seq === requestSeq) error = errorMessage(err);
		} finally {
			if (seq === requestSeq) loading = false;
		}
	}

	/** Reloads from the first page, after the filter or the order changed. */
	function restart(): void {
		firstPage(pages);
		void load();
	}

	let debounce: ReturnType<typeof setTimeout> | undefined;
	function oninput(): void {
		clearTimeout(debounce);
		debounce = setTimeout(restart, 200);
	}

	/** Sorts by `o`, or flips the direction when already sorted by it. */
	function sortBy(o: UserOrderBy): void {
		if (order === o) {
			descending = !descending;
		} else {
			order = o;
			// Newest users and site admins first; names A to Z.
			descending = o === 'CREATED_AT' || o === 'SITE_ADMIN';
		}
		restart();
	}

	/** Shows only site admins or only deactivated users; a second click shows everyone again. */
	function showOnly(which: 'admins' | 'deactivated'): void {
		show = show === which ? 'all' : which;
		restart();
	}

	/** Clears the filter text, the deactivated checkbox and the admin/deactivated filter. */
	function showAll(): void {
		clearTimeout(debounce);
		query = '';
		includeDeactivated = false;
		show = 'all';
		restart();
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

	/** The last reset link created, shown until the next one. */
	let resetLink = $state<{ username: string; url: string } | null>(null);

	async function createResetLink(user: UserNode): Promise<void> {
		pending = user.id;
		error = null;
		resetLink = null;
		created = null;
		try {
			const r = await graphql<RandomizeUserPasswordResult, { user: string }>(RANDOMIZE_USER_PASSWORD, {
				user: user.id
			});
			// Relative when `externalURL` is not set: the admin copies it, so make it absolute.
			resetLink = {
				username: user.username,
				url: new URL(r.randomizeUserPassword.resetPasswordURL, location.origin).href
			};
		} catch (err) {
			error = `Could not create a reset link for ${user.username}: ${errorMessage(err)}`;
		} finally {
			pending = null;
		}
	}

	let newUsername = $state('');
	let newEmail = $state('');
	let newEmailVerified = $state(false);
	let creating = $state(false);
	/** The last user created, with their account setup link (null when password sign-in is off). */
	let created = $state<{ username: string; url: string | null; email: boolean } | null>(null);

	async function createUser(): Promise<void> {
		creating = true;
		error = null;
		created = null;
		resetLink = null;
		const email = newEmail.trim();
		try {
			const r = await graphql<CreateUserMutation, CreateUserMutationVariables>(CREATE_USER, {
				username: newUsername.trim(),
				email: email || null,
				verifiedEmail: email !== '' && newEmailVerified
			});
			const url = r.createUser.resetPasswordURL;
			created = {
				username: r.createUser.user.username,
				url: url ? new URL(url, location.origin).href : null,
				email: email !== ''
			};
			newUsername = '';
			newEmail = '';
			newEmailVerified = false;
			await load();
		} catch (err) {
			error = `Could not create the user: ${errorMessage(err)}`;
		} finally {
			creating = false;
		}
	}

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
			Promote site admins and assign roles; roles grant permissions on the
			<a href="/site-admin/roles">roles</a> page.
		</p>
		<RelatedLinks />
	</div>
</div>

<div class="tiles" role="group" aria-label="Show users">
	<CountTile
		value={counts?.users ?? null}
		label="users"
		active={show === 'all' && !query.trim() && !includeDeactivated}
		title="Show all users and clear the filter"
		onclick={showAll}
	/>
	<CountTile
		value={counts?.siteAdmins ?? null}
		label="site admins"
		tone="primary"
		active={show === 'admins'}
		title="Show only site admins"
		onclick={() => showOnly('admins')}
	/>
	<CountTile
		value={counts?.deactivated ?? null}
		label="deactivated"
		tone={counts?.deactivated ? 'warning' : undefined}
		active={show === 'deactivated'}
		title="Show only deactivated users"
		disabled={!counts?.deactivated && show !== 'deactivated'}
		onclick={() => showOnly('deactivated')}
	/>
</div>

<section class="card create-card">
	<div class="card-header"><h2>New user</h2></div>
	<form
		class="toolbar create-row"
		onsubmit={(event) => {
			event.preventDefault();
			void createUser();
		}}
	>
		<label class="visually-hidden" for="new-username">Username</label>
		<input
			id="new-username"
			class="input"
			placeholder="Username"
			autocomplete="off"
			autocapitalize="off"
			spellcheck="false"
			bind:value={newUsername}
			required
		/>
		<label class="visually-hidden" for="new-email">Email</label>
		<input
			id="new-email"
			class="input"
			type="email"
			placeholder="Email (optional)"
			autocomplete="off"
			bind:value={newEmail}
		/>
		<button class="btn btn-primary" type="submit" disabled={creating || !newUsername.trim()}>Create user</button>
	</form>
	<label class="checkbox"
		><input type="checkbox" bind:checked={newEmailVerified} disabled={!newEmail.trim()} /> The email is verified</label
	>
	<p class="muted small hint">
		{#if data.auth.builtin}
			You get a one-time link, valid for 24 hours, for them to set a password. Mark the email verified only if you know
			it is theirs: a verified email links code host sign-ins with the same address to this account.
		{:else}
			Password sign-in is off, so they sign in with a code host. With a verified email that sign-in uses this account;
			mark it verified only if you know it is theirs.
		{/if}
	</p>
</section>

{#if siteAdminsExclusive}<ManagedNotice kind="siteAdmins" />{/if}

{#if error}
	<div class="alert alert-danger" role="alert">{error}</div>
{/if}
{#if created}
	<div
		class="alert {created.url && !(data.auth.emailEnabled && created.email) ? 'alert-warning' : 'alert-success'}"
		role="status"
	>
		{#if !created.url}
			Created {created.username}.
		{:else if data.auth.emailEnabled && created.email}
			Created {created.username} and emailed them a link to set their password. You can also send it yourself:
		{:else}
			Created {created.username}. {#if !data.auth.emailEnabled}Email is not set up (<a href={EMAIL_SETTINGS_URL}
					>set it up</a
				>), so nothing was emailed.{:else}They have no email, so nothing was emailed.{/if} Send them this one-time link to
			set their password privately:
		{/if}
		{#if created.url}<code class="mono reset-link">{created.url}</code>{/if}
	</div>
{/if}
{#if resetLink}
	<div class="alert {data.auth.emailEnabled ? 'alert-success' : 'alert-warning'}" role="status">
		{#if data.auth.emailEnabled}
			Reset link for {resetLink.username} created and emailed to their verified address, if they have one. You can also send
			it yourself:
		{:else}
			Email is not set up (<a href={EMAIL_SETTINGS_URL}>set it up</a>), so nothing was emailed. Send this single-use
			reset link to {resetLink.username} privately:
		{/if}
		<code class="mono reset-link">{resetLink.url}</code>
	</div>
{/if}

<div class="list-bar">
	<label class="visually-hidden" for="user-filter">Filter users</label>
	<input
		id="user-filter"
		class="input filter"
		type="search"
		placeholder="Filter by username or name…"
		bind:value={query}
		{oninput}
	/>
	{#if show !== 'deactivated'}
		<label class="checkbox"
			><input type="checkbox" bind:checked={includeDeactivated} onchange={restart} /> Show deactivated users</label
		>
	{/if}
	{#if show !== 'all'}
		<span class="shown">
			<strong>Only {show === 'admins' ? 'site admins' : 'deactivated users'}</strong>
			<button type="button" class="btn btn-link" onclick={showAll}>Show all users</button>
		</span>
	{/if}
	<Pager {pages} shown={users.length} total={totalCount} {loading} onchange={load} />
</div>

<div class="table-scroll">
	<table class="table table-bordered" aria-busy={loading}>
		<thead>
			<tr>
				<SortHeader key="USERNAME" label="Username" {order} {descending} onsort={sortBy} />
				<SortHeader key="DISPLAY_NAME" label="Display name" {order} {descending} onsort={sortBy} />
				<SortHeader key="CREATED_AT" label="Created" {order} {descending} onsort={sortBy} />
				<SortHeader key="SITE_ADMIN" label="Role" {order} {descending} onsort={sortBy} />
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
						{#if user.siteAdmin}<span class="badge badge-primary">Site admin</span>{:else}<span class="badge">User</span
							>{/if}
						{#each user.roles.nodes.filter((role) => !role.system) as role (role.id)}<span class="badge"
								>{role.name}</span
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
							<button class="btn btn-sm" type="button" onclick={() => editRoles(user)}>Edit roles</button>
							{#if data.auth.builtin && !user.scimControlled}
								<button
									class="btn btn-sm"
									type="button"
									disabled={pending === user.id}
									title="Create a single-use link to set a new password; the current one keeps working until it is used"
									onclick={() => createResetLink(user)}>Reset password</button
								>
							{/if}
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
								<button class="btn btn-sm" type="button" onclick={() => (editing = null)}>Cancel</button>
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

<style>
	.tiles {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(8rem, 10rem));
		gap: var(--space-2);
		margin-bottom: var(--space-5);
	}
	.list-bar {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: var(--space-2) var(--space-4);
		margin-bottom: var(--space-3);
	}
	.list-bar .shown {
		display: inline-flex;
		align-items: center;
		gap: var(--space-2);
	}
	.create-card {
		max-width: 48rem;
		margin-bottom: var(--space-6);
	}
	.create-row .input {
		flex: 1 1 12rem;
		width: auto;
	}
	.hint {
		margin: var(--space-2) 0 0;
	}
	.reset-link {
		display: block;
		margin-top: var(--space-2);
		word-break: break-all;
	}
	.list-bar .filter {
		flex: 0 1 20rem;
		width: auto;
		min-width: 12rem;
	}
	.list-bar .checkbox {
		margin-top: 0;
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
