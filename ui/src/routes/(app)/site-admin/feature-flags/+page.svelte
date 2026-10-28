<!--
	Feature flags: boolean flags and rollout flags (on for a stable share of
	users), with per-user and per-org overrides. A user override wins over org overrides, which
	win over the flag's value. Flags the resources file owns keep their overrides editable; their
	value is changed in the file.
-->
<script lang="ts">
	import { onMount } from 'svelte';
	import { formatDate, relativeTime } from '#lib/batches/batch-change.ts';
	import ManagedBadge from '#lib/components/ManagedBadge.svelte';
	import ManagedNotice from '#lib/components/ManagedNotice.svelte';
	import { isExclusive } from '#lib/declared-config.ts';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		CREATE_FEATURE_FLAG,
		CREATE_OVERRIDE,
		DELETE_FEATURE_FLAG,
		DELETE_OVERRIDE,
		FEATURE_FLAGS,
		NAMESPACE_IDS,
		UPDATE_FEATURE_FLAG,
		UPDATE_OVERRIDE,
		describeFlag,
		flagValueVariables,
		namespaceName,
		type FeatureFlag,
		type FeatureFlagsResult,
		type FlagOverride,
		type NamespaceIdsResult
	} from '#lib/admin/feature-flags.ts';

	let flags = $state.raw<FeatureFlag[]>([]);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let notice = $state<string | null>(null);
	let busy = $state(false);
	let exclusive = $state(false);

	// New flag form.
	let newName = $state('');
	let newKind = $state<'bool' | 'rollout'>('bool');
	let newValue = $state(false);
	let newPercent = $state(0);

	// Edit state, per flag name.
	let editing = $state<string | null>(null);
	let editKind = $state<'bool' | 'rollout'>('bool');
	let editValue = $state(false);
	let editPercent = $state(0);

	// New override, per flag name.
	let overrideName = $state<Record<string, string>>({});
	let overrideValue = $state<Record<string, boolean>>({});

	async function load(): Promise<void> {
		try {
			const r = await graphql<FeatureFlagsResult>(FEATURE_FLAGS);
			flags = r.featureFlags;
			exclusive = isExclusive(r.site.declaredConfigFiles, 'featureFlags');
			error = null;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loading = false;
		}
	}

	async function act(f: () => Promise<unknown>, done: string): Promise<void> {
		busy = true;
		error = null;
		notice = null;
		try {
			await f();
			notice = done;
			await load();
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = false;
		}
	}

	function create(e: SubmitEvent): void {
		e.preventDefault();
		const name = newName.trim();
		if (!name) return;
		void act(async () => {
			await graphql(CREATE_FEATURE_FLAG, { name, ...flagValueVariables(newKind, newValue, newPercent) });
			newName = '';
		}, `Created the flag ${name}.`);
	}

	function startEdit(f: FeatureFlag): void {
		editing = f.name;
		if (f.__typename === 'FeatureFlagBoolean') {
			editKind = 'bool';
			editValue = f.value;
			editPercent = 0;
		} else {
			editKind = 'rollout';
			editValue = false;
			editPercent = f.rolloutBasisPoints / 100;
		}
	}

	function saveEdit(e: SubmitEvent, name: string): void {
		e.preventDefault();
		void act(async () => {
			await graphql(UPDATE_FEATURE_FLAG, { name, ...flagValueVariables(editKind, editValue, editPercent) });
			editing = null;
		}, `Updated the flag ${name}.`);
	}

	function remove(f: FeatureFlag): void {
		if (!confirm(`Delete the flag ${f.name} and its ${f.overrides.length} overrides?`)) return;
		void act(() => graphql(DELETE_FEATURE_FLAG, { name: f.name }), `Deleted the flag ${f.name}.`);
	}

	function addOverride(e: SubmitEvent, flag: string): void {
		e.preventDefault();
		const name = (overrideName[flag] ?? '').trim();
		if (!name) return;
		void act(async () => {
			const ns = await graphql<NamespaceIdsResult, { name: string }>(NAMESPACE_IDS, { name });
			const id = ns.user?.id ?? ns.organization?.id;
			if (!id) throw new Error(`No user or organization is named ${name}.`);
			await graphql(CREATE_OVERRIDE, { namespace: id, flagName: flag, value: overrideValue[flag] ?? true });
			overrideName = { ...overrideName, [flag]: '' };
		}, `Added an override of ${flag} for ${name}.`);
	}

	const toggleOverride = (o: FlagOverride) =>
		void act(
			() => graphql(UPDATE_OVERRIDE, { id: o.id, value: !o.value }),
			`The override for ${namespaceName(o)} is now ${o.value ? 'off' : 'on'}.`
		);

	const removeOverride = (o: FlagOverride) =>
		void act(() => graphql(DELETE_OVERRIDE, { id: o.id }), `Removed the override for ${namespaceName(o)}.`);

	onMount(() => void load());
</script>

<svelte:head><title>Feature flags · Site admin · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Feature flags</h1>
		<p class="page-description">
			Boolean flags are on or off for everyone; rollout flags are on for a stable share of signed-in
			users. Overrides set a flag for one user or for the members of an organization: a user override
			wins, then org overrides (on wins when orgs disagree), then the flag.
		</p>
	</div>
</div>

{#if notice}<div class="alert alert-info" role="status">{notice}</div>{/if}
{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}

{#if exclusive}
	<ManagedNotice kind="featureFlags" />
{:else}
	<form class="card new-flag" onsubmit={create}>
		<div class="card-header"><h2>New flag</h2></div>
		<div class="row">
			<label class="field">
				<span class="label">Name</span>
				<input class="input" bind:value={newName} required placeholder="my-feature" />
			</label>
			<label class="field">
				<span class="label">Type</span>
				<select class="input" bind:value={newKind}>
					<option value="bool">Boolean</option>
					<option value="rollout">Rollout</option>
				</select>
			</label>
			{#if newKind === 'bool'}
				<label class="checkbox"><input type="checkbox" bind:checked={newValue} /> On</label>
			{:else}
				<label class="field">
					<span class="label">Users (%)</span>
					<input class="input pct" type="number" min="0" max="100" step="0.01" bind:value={newPercent} />
				</label>
			{/if}
			<button class="btn btn-primary" type="submit" disabled={busy || !newName.trim()}>Create</button>
		</div>
	</form>
{/if}

<div class="section-header">
	<h2>Flags</h2>
	{#if flags.length > 0}<span class="muted small">{flags.length} flag{flags.length === 1 ? '' : 's'}</span>{/if}
</div>

{#if loading && flags.length === 0}<p class="loading" aria-live="polite"><span class="spinner"></span> Loading…</p>{/if}
{#if !loading && flags.length === 0}
	<div class="empty-state">
		<h3>No feature flags yet</h3>
		{#if exclusive}
			<p>Declare flags in the <code class="code-inline">featureFlags</code> section of SOURCEGULL_RESOURCES_FILE.</p>
		{:else}
			<p>Create a flag above to turn a feature on for everyone, a share of users, or one user or organization.</p>
		{/if}
	</div>
{/if}

{#each flags as f (f.name)}
	<article class="card flag" data-flag={f.name}>
		<header>
			<h3 class="mono">{f.name}</h3>
			<span class="badge badge-info">{f.__typename === 'FeatureFlagBoolean' ? 'boolean' : 'rollout'}</span>
			<ManagedBadge managedBy={f.managedBy} />
			<strong data-testid="flag-value">{describeFlag(f)}</strong>
			<span class="muted small" title={formatDate(f.updatedAt)}>updated {relativeTime(f.updatedAt)}</span>
			<span class="spacer"></span>
			{#if !f.managedBy}
				<button class="btn btn-sm" type="button" onclick={() => startEdit(f)} disabled={busy}>Edit</button>
				<button class="btn btn-sm btn-danger" type="button" onclick={() => remove(f)} disabled={busy}>Delete</button>
			{/if}
		</header>

		{#if f.managedBy}
			<div class="managed"><ManagedNotice managedBy={f.managedBy} /></div>
		{/if}

		{#if editing === f.name && !f.managedBy}
			<form class="row edit" onsubmit={(e) => saveEdit(e, f.name)}>
				<select class="input" bind:value={editKind} aria-label="Type">
					<option value="bool">Boolean</option>
					<option value="rollout">Rollout</option>
				</select>
				{#if editKind === 'bool'}
					<label class="checkbox"><input type="checkbox" bind:checked={editValue} /> On</label>
				{:else}
					<label class="field">
						<span class="label">Users (%)</span>
						<input class="input pct" type="number" min="0" max="100" step="0.01" bind:value={editPercent} />
					</label>
				{/if}
				<button class="btn btn-primary btn-sm" type="submit" disabled={busy}>Save</button>
				<button class="btn btn-sm" type="button" onclick={() => (editing = null)}>Cancel</button>
			</form>
		{/if}

		<h4>Overrides</h4>
		{#if f.overrides.length > 0}
			<ul class="overrides">
				{#each f.overrides as o (o.id)}
					<li>
						<span class="badge">{o.namespace.__typename === 'User' ? 'user' : 'org'}</span>
						<span class="mono">{namespaceName(o)}</span>
						<span class="badge" class:badge-success={o.value}>{o.value ? 'on' : 'off'}</span>
						<button class="btn btn-sm" type="button" onclick={() => toggleOverride(o)} disabled={busy}
							>Turn {o.value ? 'off' : 'on'}</button
						>
						<button class="btn btn-sm" type="button" onclick={() => removeOverride(o)} disabled={busy}>Remove</button>
					</li>
				{/each}
			</ul>
		{:else}
			<p class="muted small">None.</p>
		{/if}
		<form class="row add-override" onsubmit={(e) => addOverride(e, f.name)}>
			<input
				class="input"
				placeholder="Username or organization name"
				aria-label="Username or organization name for {f.name}"
				value={overrideName[f.name] ?? ''}
				oninput={(e) => (overrideName = { ...overrideName, [f.name]: e.currentTarget.value })}
			/>
			<label class="checkbox">
				<input
					type="checkbox"
					checked={overrideValue[f.name] ?? true}
					onchange={(e) => (overrideValue = { ...overrideValue, [f.name]: e.currentTarget.checked })}
				/> On
			</label>
			<button class="btn btn-sm" type="submit" disabled={busy}>Add override</button>
		</form>
	</article>
{/each}

<style>
	.new-flag,
	.flag {
		margin-bottom: var(--space-3);
	}
	.new-flag {
		max-width: 48rem;
	}
	.row {
		display: flex;
		flex-wrap: wrap;
		align-items: flex-end;
		gap: var(--space-2) var(--space-3);
	}
	.row .field {
		margin-bottom: 0;
	}
	.row .checkbox {
		min-height: var(--control-height);
	}
	.flag header {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: var(--space-2);
	}
	.flag h3 {
		margin: 0;
		font-size: var(--text-md);
	}
	.flag h4 {
		margin: var(--space-4) 0 var(--space-2);
		font-size: var(--text-xs);
		font-weight: var(--weight-medium);
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: var(--color-text-muted);
	}
	.spacer {
		flex: 1;
	}
	.managed {
		margin-top: var(--space-3);
	}
	.managed :global(.alert) {
		margin-bottom: 0;
	}
	.edit {
		margin-top: var(--space-3);
		padding-top: var(--space-3);
		border-top: 1px solid var(--color-border);
	}
	.pct {
		width: 7rem;
	}
	.overrides {
		list-style: none;
		padding: 0;
		margin: 0 0 var(--space-2);
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
	}
	.overrides li {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: var(--space-2);
		padding: var(--space-2) var(--space-3);
	}
	.overrides li + li {
		border-top: 1px solid var(--color-border);
	}
	.add-override .input {
		flex: 1 1 14rem;
		max-width: 24rem;
	}
</style>
