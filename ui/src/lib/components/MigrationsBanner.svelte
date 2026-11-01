<!--
	Tells a site admin that background data migrations are still running, and which of them must
	finish before the next upgrade. Checked once when shown; failures are silent (the banner is a
	hint and the Migrations page has the details).
-->
<script lang="ts">
	import { onMount } from 'svelte';
	import { graphql } from '#lib/graphql.ts';
	import {
		RUNNING_MIGRATIONS,
		percent,
		runningMigrations,
		type RunningMigration,
		type RunningMigrationsResult
	} from '#lib/updates/migrations.ts';

	let running = $state.raw<RunningMigration[]>([]);
	const blocking = $derived(running.some((m) => m.requiredBeforeNextUpgrade));

	onMount(() => {
		graphql<RunningMigrationsResult>(RUNNING_MIGRATIONS)
			.then((r) => (running = runningMigrations(r.outOfBandMigrations)))
			.catch(() => {});
	});
</script>

{#if running.length > 0}
	<div class="alert {blocking ? 'alert-warning' : 'alert-info'} banner" role="status">
		<div>
			<strong>Background migrations are running.</strong>
			{#each running as m, i (m.id)}{i > 0 ? '; ' : ' '}{m.description}
				({percent(m.progress)}%{m.applyReverse ? ', in reverse' : ''}){/each}.
			{#if blocking}
				Wait for them to finish before upgrading to the next version.
			{/if}
		</div>
		<a class="btn btn-sm btn-secondary" href="/site-admin/migrations">Migrations</a>
	</div>
{/if}

<style>
	.banner {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--space-3);
		margin-bottom: var(--space-4);
		text-align: left;
	}
	.banner .btn {
		flex-shrink: 0;
	}
</style>
