<script lang="ts">
	import { onMount } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		UPGRADE_READINESS,
		allStatements,
		type UpgradeReadinessResult
	} from '#lib/updates/queries.ts';
	import { percent } from '#lib/updates/migrations.ts';

	let result = $state.raw<UpgradeReadinessResult['site'] | null>(null);
	let loading = $state(true);
	let error = $state<string | null>(null);

	async function load(): Promise<void> {
		loading = true;
		error = null;
		try {
			result = (await graphql<UpgradeReadinessResult>(UPGRADE_READINESS)).site;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loading = false;
		}
	}

	const readiness = $derived(result?.upgradeReadiness);

	onMount(() => void load());
</script>

<svelte:head><title>Updates · Site admin · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Updates</h1>
		<p class="page-description">
			The running version, and whether the database is ready for the next upgrade.
		</p>
	</div>
	<div class="page-actions">
		<button class="btn" type="button" disabled={loading} onclick={() => void load()}>Check again</button>
	</div>
</div>

{#if error}
	<div class="alert alert-danger" role="alert">{error}</div>
{/if}

{#if result && readiness}
	<dl class="card facts">
		<div>
			<dt>Sourcegull</dt>
			<dd class="mono">{result.productVersion}</dd>
		</div>
		{#if readiness.schemaVersion}
			<div>
				<dt>Database schema</dt>
				<dd class="mono">{readiness.schemaVersion}</dd>
			</div>
		{/if}
	</dl>

	<div class="section-header"><h2>Upgrade readiness</h2></div>
	<p class="muted intro">
		Upgrades check the database schema against the schema its version shipped with. Differences
		(drift) come from manual changes or migrations repaired by hand and can make upgrades fail.
		<code>sourcegull migrate drift</code> prints the same report; upgrade across several versions with
		<code>sourcegull migrate upgrade --from &lt;version&gt;</code>.
	</p>

	{#if readiness.schemaDriftNote}
		<div class="alert alert-warning" role="status">
			The schema could not be checked: {readiness.schemaDriftNote}
		</div>
	{:else if readiness.schemaDrift.length === 0}
		<div class="alert alert-success" role="status">No schema drift: the database is ready.</div>
	{:else}
		<div class="alert alert-danger" role="status">
			Schema drift: {readiness.schemaDrift.length} difference{readiness.schemaDrift.length === 1
				? ''
				: 's'}. Review the statements before running them; dropping a column loses its data.
		</div>
		<ul class="drift">
			{#each readiness.schemaDrift as d, i (i)}
				<li>
					<div><strong class="mono">{d.name}</strong>: {d.problem}</div>
					<div class="muted small">{d.solution}</div>
					{#if d.diff}
						<pre class="code" aria-label="Difference">{d.diff}</pre>
					{/if}
					{#if d.statements}
						<pre class="code" aria-label="Fix statements">{d.statements.join('\n')}</pre>
					{/if}
				</li>
			{/each}
		</ul>
		<details>
			<summary>All statements</summary>
			<pre class="code">{allStatements(readiness.schemaDrift)}</pre>
		</details>
	{/if}
	<div class="section-header"><h2>Out-of-band migrations</h2></div>
	{#if readiness.requiredOutOfBandMigrations.length === 0}
		<div class="alert alert-success" role="status">
			No unfinished out-of-band migrations block the next upgrade.
		</div>
	{:else}
		<div class="alert alert-warning" role="status">
			These migrations must finish before upgrading to the next version (see
			<a href="/site-admin/migrations">Migrations</a>):
		</div>
		<ul class="required">
			{#each readiness.requiredOutOfBandMigrations as m (m.id)}
				<li>
					{m.description}: {percent(m.progress)}%{m.applyReverse ? ', applied in reverse' : ''}
					{#if m.deprecated}<span class="muted">(deprecated in {m.deprecated})</span>{/if}
				</li>
			{/each}
		</ul>
	{/if}
{:else if loading}
	<p class="loading"><span class="spinner"></span> Loading…</p>
{/if}

<style>
	.facts {
		display: flex;
		flex-wrap: wrap;
		gap: var(--space-4) var(--space-10);
		margin: 0;
		padding: var(--space-4) var(--space-6);
	}
	.facts dt {
		font-size: var(--text-xs);
		font-weight: var(--weight-medium);
		text-transform: uppercase;
		letter-spacing: 0.04em;
		color: var(--color-text-muted);
	}
	.facts dd {
		margin: var(--space-1) 0 0;
		font-size: var(--text-lg);
		font-weight: var(--weight-semibold);
		overflow-wrap: anywhere;
	}
	.intro {
		max-width: 48rem;
	}
	.drift {
		list-style: none;
		padding: 0;
		display: grid;
		gap: var(--space-3);
	}
	.drift li {
		border: 1px solid var(--color-border);
		border-radius: var(--radius-lg);
		background: var(--color-surface);
		padding: var(--space-3) var(--space-4);
		min-width: 0;
	}
	.required li + li {
		margin-top: var(--space-1);
	}
	pre.code {
		margin: var(--space-2) 0 0;
		padding: var(--space-2) var(--space-3);
		background: var(--color-bg-muted);
		border-radius: var(--radius);
		overflow-x: auto;
		white-space: pre;
		font-size: var(--text-xs);
	}
</style>
