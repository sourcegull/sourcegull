<!--
	Roles and their permissions: create custom roles, change which permissions a role grants, and
	delete custom roles. Roles the resources file owns are shown read-only, and when it owns every
	custom role none can be created.
-->
<script lang="ts">
	import RelatedLinks from '#lib/components/RelatedLinks.svelte';
	import { onMount } from 'svelte';
	import ManagedBadge from '#lib/components/ManagedBadge.svelte';
	import ManagedNotice from '#lib/components/ManagedNotice.svelte';
	import { isExclusive } from '#lib/declared-config.ts';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		RBAC,
		CREATE_ROLE,
		DELETE_ROLE,
		SET_PERMISSIONS,
		type RbacResult,
		type RoleNode,
		type PermissionNode
	} from '#lib/queries.ts';

	let roles = $state<RoleNode[]>([]);
	let permissions = $state<PermissionNode[]>([]);
	let chosen = $state<Record<string, string[]>>({});
	let name = $state('');
	let busy = $state<string | null>(null);
	let error = $state<string | null>(null);
	let loading = $state(true);
	let exclusive = $state(false);

	async function load(): Promise<void> {
		loading = true;
		try {
			const result = await graphql<RbacResult>(RBAC);
			roles = result.roles.nodes;
			permissions = result.permissions.nodes;
			exclusive = isExclusive(result.site.declaredConfigFiles, 'roles');
			chosen = Object.fromEntries(
				roles.map((role) => [role.id, role.permissions.nodes.map((p) => p.id)])
			);
			error = null;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loading = false;
		}
	}

	function toggle(role: string, permission: string): void {
		const selected = new Set(chosen[role] ?? []);
		if (selected.has(permission)) selected.delete(permission);
		else selected.add(permission);
		chosen[role] = [...selected];
	}

	async function save(role: RoleNode): Promise<void> {
		busy = role.id;
		try {
			await graphql(SET_PERMISSIONS, { role: role.id, permissions: chosen[role.id] ?? [] });
			await load();
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = null;
		}
	}

	async function create(): Promise<void> {
		if (!name.trim()) return;
		busy = 'create';
		try {
			await graphql(CREATE_ROLE, { name: name.trim(), permissions: [] });
			name = '';
			await load();
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = null;
		}
	}

	async function remove(role: RoleNode): Promise<void> {
		if (!confirm(`Delete role ${role.name}?`)) return;
		busy = role.id;
		try {
			await graphql(DELETE_ROLE, { role: role.id });
			await load();
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = null;
		}
	}

	let namespaces = $derived([...new Set(permissions.map((p) => p.namespace))]);
	onMount(() => {
		void load();
	});
</script>

<svelte:head><title>Roles · Site admin · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Roles</h1>
		<p class="page-description">
			Roles bundle permissions. Assign them to people on the <a href="/site-admin/users">users</a>
			page; system roles are built in and cannot be deleted.
		</p>
		<RelatedLinks />
	</div>
</div>
{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}

{#if exclusive}
	<ManagedNotice kind="roles" />
{:else}
	<form
		class="card create-row"
		onsubmit={(event) => {
			event.preventDefault();
			void create();
		}}
	>
		<label for="role-name">New custom role</label>
		<input id="role-name" class="input" placeholder="Role name" bind:value={name} required />
		<button class="btn btn-primary" type="submit" disabled={busy !== null}>Create role</button>
	</form>
{/if}

{#if loading}<p class="loading"><span class="spinner"></span> Loading…</p>{/if}
{#each roles as role (role.id)}
	<section class="card role-card">
		<div class="card-header role-header">
			<h2>
				{role.name}
				{#if role.system}<span class="badge">System</span>{/if}
				<ManagedBadge managedBy={role.managedBy} />
			</h2>
			{#if !role.managedBy}
				<div class="role-actions">
					<button
						class="btn btn-sm btn-primary"
						type="button"
						disabled={busy !== null}
						onclick={() => save(role)}>Save permissions</button
					>
					{#if !role.system}<button
							class="btn btn-sm btn-danger"
							type="button"
							disabled={busy !== null}
							onclick={() => remove(role)}>Delete</button
						>{/if}
				</div>
			{/if}
		</div>
		<ManagedNotice managedBy={role.managedBy} />
		<div class="permission-groups">
			{#each namespaces as namespace}
				<fieldset>
					<legend>{namespace}</legend>
					{#each permissions.filter((p) => p.namespace === namespace) as permission (permission.id)}
						<label
							><input
								type="checkbox"
								checked={(chosen[role.id] ?? []).includes(permission.id)}
								disabled={role.managedBy !== null}
								onchange={() => toggle(role.id, permission.id)}
							/>
							{permission.action}</label
						>
					{/each}
				</fieldset>
			{/each}
		</div>
	</section>
{/each}

<style>
	.create-row {
		display: flex;
		align-items: center;
		gap: var(--space-3);
		flex-wrap: wrap;
		max-width: 48rem;
		margin-bottom: var(--space-6);
	}
	.create-row label {
		font-weight: var(--weight-medium);
	}
	.create-row .input {
		flex: 1 1 12rem;
		width: auto;
	}
	.role-header {
		flex-wrap: wrap;
	}
	.role-header h2 {
		display: flex;
		align-items: center;
		gap: var(--space-2);
	}
	.role-actions {
		display: flex;
		gap: var(--space-2);
	}
	.role-card {
		margin: 0 0 var(--space-4);
	}
	.permission-groups {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(10rem, 1fr));
		gap: var(--space-4);
	}
	legend {
		padding: 0;
		margin-bottom: var(--space-1);
		font-size: var(--text-xs);
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: var(--color-text-muted);
	}
	fieldset {
		border: 0;
		padding: 0;
		min-width: 9rem;
	}
	fieldset label {
		display: block;
		margin: var(--space-1) 0;
	}
</style>
