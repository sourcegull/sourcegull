<!--
	Executor secrets of the global namespace (site admin) or of the signed-in user: list,
	create, replace a value, delete, and each secret's access logs. Values are write-only: the API
	never returns them. In a user's namespace the global secrets are listed too, read-only. Global
	secrets the resources file owns are read-only, and when it owns them all none can be added.
-->
<script lang="ts">
	import RelatedLinks from '#lib/components/RelatedLinks.svelte';
	import { onMount } from 'svelte';
	import ManagedBadge from '#lib/components/ManagedBadge.svelte';
	import ManagedNotice from '#lib/components/ManagedNotice.svelte';
	import { isExclusive } from '#lib/declared-config.ts';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		CREATE_EXECUTOR_SECRET,
		DELETE_EXECUTOR_SECRET,
		EXECUTOR_SECRET_ACCESS_LOGS,
		GLOBAL_EXECUTOR_SECRETS,
		UPDATE_EXECUTOR_SECRET,
		USER_EXECUTOR_SECRETS,
		type CreateExecutorSecretVariables,
		type ExecutorSecretAccessLogNode,
		type ExecutorSecretAccessLogsResult,
		type ExecutorSecretNode,
		type ExecutorSecretScope,
		type GlobalExecutorSecretsResult,
		type UserExecutorSecretsResult
	} from '#lib/queries.ts';

	interface Props {
		/** `global`: the site's secrets; `user`: the signed-in user's. */
		namespace: 'global' | 'user';
		/** The scopes offered, the first selected. */
		scopes: ExecutorSecretScope[];
	}

	let { namespace, scopes }: Props = $props();

	type ListVariables = { scope: ExecutorSecretScope; first: number };
	type SecretVariables = { scope: ExecutorSecretScope; id: string };

	const SCOPE_TITLES: Record<ExecutorSecretScope, string> = {
		CODEINTEL: 'Auto-indexing',
		BATCHES: 'Bulk changes'
	};

	// The scopes are fixed per page.
	// svelte-ignore state_referenced_locally
	let scope = $state<ExecutorSecretScope>(scopes[0] ?? 'CODEINTEL');
	let secrets = $state<ExecutorSecretNode[]>([]);
	let totalCount = $state(0);
	let namespaceId = $state<string | null>(null);
	let loading = $state(true);
	let error = $state<string | null>(null);
	/** Whether the resources file owns every global secret. */
	let exclusive = $state(false);

	let creating = $state(false);
	let key = $state('');
	let value = $state('');
	let submitting = $state(false);

	/** The secret whose value is being replaced. */
	let editing = $state<string | null>(null);
	let newValue = $state('');

	/** The secret whose access logs are shown. */
	let logsFor = $state<ExecutorSecretNode | null>(null);
	let logs = $state<ExecutorSecretAccessLogNode[]>([]);

	async function load(): Promise<void> {
		loading = true;
		error = null;
		try {
			if (namespace === 'global') {
				const data = await graphql<GlobalExecutorSecretsResult, ListVariables>(
					GLOBAL_EXECUTOR_SECRETS,
					{ scope, first: 500 }
				);
				secrets = data.executorSecrets.nodes;
				totalCount = data.executorSecrets.totalCount;
				exclusive = isExclusive(data.site.declaredConfigFiles, 'executorSecrets');
			} else {
				const data = await graphql<UserExecutorSecretsResult, ListVariables>(
					USER_EXECUTOR_SECRETS,
					{ scope, first: 500 }
				);
				namespaceId = data.currentUser?.id ?? null;
				secrets = data.currentUser?.executorSecrets.nodes ?? [];
				totalCount = data.currentUser?.executorSecrets.totalCount ?? 0;
			}
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loading = false;
		}
	}

	/**
	 * Whether the secret belongs to the namespace shown (global ones are read-only for users) and
	 * is not owned by the resources file.
	 */
	const editable = (s: ExecutorSecretNode) =>
		(namespace === 'global') === (s.namespace === null) && !s.managedBy;

	async function create(event: SubmitEvent): Promise<void> {
		event.preventDefault();
		submitting = true;
		error = null;
		try {
			await graphql<unknown, CreateExecutorSecretVariables>(CREATE_EXECUTOR_SECRET, {
				scope,
				key: key.trim(),
				value,
				namespace: namespace === 'user' ? namespaceId : null
			});
			creating = false;
			key = '';
			value = '';
			await load();
		} catch (err) {
			error = errorMessage(err);
		} finally {
			submitting = false;
		}
	}

	async function update(event: SubmitEvent, secret: ExecutorSecretNode): Promise<void> {
		event.preventDefault();
		submitting = true;
		error = null;
		try {
			await graphql<unknown, SecretVariables & { value: string }>(UPDATE_EXECUTOR_SECRET, {
				scope: secret.scope,
				id: secret.id,
				value: newValue
			});
			editing = null;
			newValue = '';
			await load();
		} catch (err) {
			error = errorMessage(err);
		} finally {
			submitting = false;
		}
	}

	async function remove(secret: ExecutorSecretNode): Promise<void> {
		if (!confirm(`Delete the executor secret ${secret.key}? Jobs will no longer get it.`)) return;
		error = null;
		try {
			await graphql<unknown, SecretVariables>(DELETE_EXECUTOR_SECRET, {
				scope: secret.scope,
				id: secret.id
			});
			if (logsFor?.id === secret.id) logsFor = null;
			await load();
		} catch (err) {
			error = errorMessage(err);
		}
	}

	async function showLogs(secret: ExecutorSecretNode): Promise<void> {
		error = null;
		try {
			const data = await graphql<ExecutorSecretAccessLogsResult, { id: string; first: number }>(
				EXECUTOR_SECRET_ACCESS_LOGS,
				{ id: secret.id, first: 100 }
			);
			logs = data.node?.accessLogs.nodes ?? [];
			logsFor = secret;
		} catch (err) {
			error = errorMessage(err);
		}
	}

	function selectScope(s: ExecutorSecretScope): void {
		scope = s;
		logsFor = null;
		editing = null;
		void load();
	}

	const formatDate = (iso: string) => new Date(iso).toLocaleString();

	onMount(() => {
		void load();
	});
</script>

<div class="page-header">
	<div>
		<h1>Executor secrets</h1>
		<RelatedLinks />
	</div>
	{#if !creating && !exclusive}
		<button class="btn btn-primary" type="button" onclick={() => (creating = true)}>Add secret</button>
	{/if}
</div>

<p class="muted">
	Executor secrets are passed to executor jobs as environment variables named by their key, and
	their values are replaced in the jobs' logs. Values are encrypted and can't be read back. A secret
	named <code>DOCKER_AUTH_CONFIG</code> holds registry credentials for pulling the jobs' images (a
	Docker <code>config.json</code> with <code>auths</code>).
	{#if namespace === 'user'}
		Your secrets overwrite global secrets with the same key.
	{/if}
</p>

{#if scopes.length > 1}
	<div class="scopes" role="tablist" aria-label="Scope">
		{#each scopes as s (s)}
			<button
				class="btn btn-sm"
				class:btn-primary={scope === s}
				type="button"
				role="tab"
				aria-selected={scope === s}
				onclick={() => selectScope(s)}>{SCOPE_TITLES[s]}</button
			>
		{/each}
	</div>
{/if}

{#if exclusive}<ManagedNotice kind="executorSecrets" />{/if}

{#if error}
	<div class="alert alert-danger" role="alert">{error}</div>
{/if}

{#if creating && !exclusive}
	<form class="card create" onsubmit={create}>
		<h2>New {SCOPE_TITLES[scope].toLowerCase()} secret</h2>
		<div class="field">
			<label for="secret-key">Key</label>
			<!-- svelte-ignore a11y_autofocus -->
			<input
				id="secret-key"
				class="input mono"
				bind:value={key}
				required
				pattern="[A-Z][A-Z0-9_]*"
				placeholder="GITHUB_TOKEN"
				autofocus
			/>
			<div class="muted small">Uppercase letters, digits and underscores, starting with a letter.</div>
		</div>
		<div class="field">
			<label for="secret-value">Value</label>
			<textarea id="secret-value" class="input mono" rows="3" bind:value required></textarea>
		</div>
		<div class="toolbar">
			<button class="btn btn-primary" type="submit" disabled={submitting || !key.trim() || !value}>
				{submitting ? 'Adding…' : 'Add secret'}
			</button>
			<button class="btn" type="button" onclick={() => (creating = false)}>Cancel</button>
		</div>
	</form>
{/if}

<div class="table-scroll">
	<table class="table" aria-busy={loading}>
		<thead>
			<tr>
				<th scope="col">Key</th>
				<th scope="col">Created by</th>
				<th scope="col">Updated</th>
				<th scope="col">Accesses</th>
				<th scope="col"><span class="visually-hidden">Actions</span></th>
			</tr>
		</thead>
		<tbody>
			{#each secrets as secret (secret.id)}
				<tr data-secret={secret.key}>
					<td>
						<code>{secret.key}</code>
						{#if secret.namespace === null && namespace === 'user'}
							<span class="badge">global</span>
						{/if}
						{#if secret.overwritesGlobalSecret}
							<span class="badge">overwrites global</span>
						{/if}
						{#if namespace === 'global'}<ManagedBadge managedBy={secret.managedBy} />{/if}
					</td>
					<td>{secret.creator?.username ?? '—'}</td>
					<td>{formatDate(secret.updatedAt)}</td>
					<td>{secret.accessLogs.totalCount}</td>
					<td class="actions">
						<button class="btn btn-sm" type="button" onclick={() => showLogs(secret)}>Access logs</button>
						{#if editable(secret)}
							<button
								class="btn btn-sm"
								type="button"
								onclick={() => {
									editing = secret.id;
									newValue = '';
								}}>Update value</button
							>
							<button class="btn btn-sm btn-danger" type="button" onclick={() => remove(secret)}
								>Delete</button
							>
						{/if}
					</td>
				</tr>
				{#if editing === secret.id}
					<tr>
						<td colspan="5">
							<form class="update" onsubmit={(e) => update(e, secret)}>
								<label for="secret-new-value">New value of <code>{secret.key}</code></label>
								<textarea
									id="secret-new-value"
									class="input mono"
									rows="3"
									bind:value={newValue}
									required
								></textarea>
								<div class="toolbar">
									<button class="btn btn-primary btn-sm" type="submit" disabled={submitting || !newValue}
										>Save</button
									>
									<button class="btn btn-sm" type="button" onclick={() => (editing = null)}>Cancel</button>
								</div>
							</form>
						</td>
					</tr>
				{/if}
			{:else}
				<tr>
					<td colspan="5" class="muted">{loading ? 'Loading…' : 'No executor secrets.'}</td>
				</tr>
			{/each}
		</tbody>
	</table>
</div>
<p class="muted small">{totalCount} {totalCount === 1 ? 'secret' : 'secrets'}</p>

{#if logsFor}
	<section class="logs" aria-label="Access logs">
		<div class="page-header">
			<h2>Access logs of <code>{logsFor.key}</code></h2>
			<button class="btn btn-sm" type="button" onclick={() => (logsFor = null)}>Close</button>
		</div>
		<table class="table">
			<thead>
				<tr>
					<th scope="col">Accessed by</th>
					<th scope="col">When</th>
				</tr>
			</thead>
			<tbody>
				{#each logs as log (log.id)}
					<tr data-access-log>
						<td>{log.user?.username ?? (log.machineUser || 'deleted user')}</td>
						<td>{formatDate(log.createdAt)}</td>
					</tr>
				{:else}
					<tr><td colspan="2" class="muted">The value has not been read yet.</td></tr>
				{/each}
			</tbody>
		</table>
	</section>
{/if}

<style>
	.scopes {
		display: flex;
		gap: var(--space-2);
		margin-bottom: var(--space-3);
	}
	.create {
		margin-bottom: var(--space-6);
		max-width: 36rem;
	}
	.update {
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
		max-width: 36rem;
	}
	.actions {
		text-align: right;
		white-space: nowrap;
	}
	.badge {
		margin-left: var(--space-1);
	}
	.small {
		font-size: 0.875em;
	}
	.logs {
		margin-top: var(--space-6);
	}
</style>
