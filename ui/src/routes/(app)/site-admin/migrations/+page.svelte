<script lang="ts">
	import { onDestroy, onMount } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		OUT_OF_BAND_MIGRATIONS,
		SET_MIGRATION_DIRECTION,
		migrationState,
		percent,
		type OutOfBandMigration,
		type OutOfBandMigrationsResult
	} from '#lib/updates/migrations.ts';

	/** Progress refreshes this often while the page is open. */
	const REFRESH_MS = 10_000;

	let migrations = $state.raw<OutOfBandMigration[]>([]);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let busy = $state<string | null>(null);
	let timer: ReturnType<typeof setInterval> | undefined;

	async function load(): Promise<void> {
		try {
			migrations = (await graphql<OutOfBandMigrationsResult>(OUT_OF_BAND_MIGRATIONS))
				.outOfBandMigrations;
			error = null;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loading = false;
		}
	}

	async function setDirection(m: OutOfBandMigration, applyReverse: boolean): Promise<void> {
		busy = m.id;
		try {
			await graphql<unknown, { id: string; applyReverse: boolean }>(SET_MIGRATION_DIRECTION, {
				id: m.id,
				applyReverse
			});
			await load();
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = null;
		}
	}

	onMount(() => {
		void load();
		timer = setInterval(() => void load(), REFRESH_MS);
	});
	onDestroy(() => clearInterval(timer));
</script>

<svelte:head><title>Migrations · Site admin · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Out-of-band migrations</h1>
		<p class="page-description">
			Long-running data migrations that workers run in the background after an upgrade, such as
			re-encrypting secrets after a key rotation. A release that deprecates a migration cannot be
			installed until the migration reaches 100%. <code>sourcegull migrate status</code> lists them too.
		</p>
	</div>
</div>

{#if error}
	<div class="alert alert-danger" role="alert">{error}</div>
{/if}

<div class="table-scroll">
	<table class="table table-bordered" aria-busy={loading}>
		<thead>
			<tr>
				<th scope="col">Migration</th>
				<th scope="col">Progress</th>
				<th scope="col">Versions</th>
				<th scope="col">Direction</th>
			</tr>
		</thead>
		<tbody>
			{#each migrations as m (m.id)}
				<tr>
					<td>
						<strong>{m.description}</strong>
						<div class="muted small">
							{m.component} · {m.team}{#if !m.nonDestructive}
								<span class="badge badge-danger">destructive</span>{/if}
						</div>
						{#if m.errors.length > 0}
							<details class="errors">
								<summary
									>{m.errors.length} recent error{m.errors.length === 1 ? '' : 's'}</summary
								>
								<ul>
									{#each m.errors as e, i (i)}
										<li>
											<span class="muted small">{new Date(e.created).toLocaleString()}</span>
											<pre class="code">{e.message}</pre>
										</li>
									{/each}
								</ul>
							</details>
						{/if}
					</td>
					<td class="progress-cell">
						<progress max="100" value={percent(m.progress)} aria-label="Progress of {m.description}"
						></progress>
						<div class="small">
							{percent(m.progress)}% · {migrationState(m)}
						</div>
						{#if m.lastUpdated}
							<div class="muted small">updated {new Date(m.lastUpdated).toLocaleString()}</div>
						{/if}
					</td>
					<td class="small">
						<div>introduced in {m.introduced}</div>
						<div>
							{#if m.deprecated}
								<span class="badge badge-warning">must finish before {m.deprecated}</span>
							{:else}
								<span class="muted">not deprecated</span>
							{/if}
						</div>
					</td>
					<td>
						<button
							class="btn btn-sm"
							type="button"
							disabled={busy === m.id}
							title={m.applyReverse
								? 'Migrate the data forward again'
								: 'Move the data back to the previous format before a downgrade'}
							onclick={() => void setDirection(m, !m.applyReverse)}
							>{m.applyReverse ? 'Run forward' : 'Apply in reverse'}</button
						>
					</td>
				</tr>
			{:else}
				<tr>
					<td colspan="4" class="empty-cell"
						>{loading
							? 'Loading…'
							: 'No out-of-band migrations are registered yet (they register when Sourcegull starts).'}</td
					>
				</tr>
			{/each}
		</tbody>
	</table>
</div>

<style>
	.progress-cell {
		min-width: 10rem;
	}
	progress {
		width: 100%;
		accent-color: var(--color-primary);
	}
	.errors ul {
		list-style: none;
		padding: 0;
		margin: var(--space-2) 0 0;
	}
	pre.code {
		margin: var(--space-1) 0 var(--space-2);
		padding: var(--space-2);
		background: var(--color-bg-muted);
		border-radius: var(--radius);
		white-space: pre-wrap;
		overflow-wrap: anywhere;
		font-size: var(--text-xs);
	}
</style>
