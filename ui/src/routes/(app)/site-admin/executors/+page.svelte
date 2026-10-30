<script lang="ts">
	import RelatedLinks from '#lib/components/RelatedLinks.svelte';
	import { onDestroy, onMount } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import { EXECUTORS, type ExecutorNode, type ExecutorsResult } from '#lib/queries.ts';

	/** The list refreshes this often (executors send a heartbeat every 5 seconds). */
	const REFRESH_MS = 10_000;

	let executors = $state<ExecutorNode[]>([]);
	let totalCount = $state(0);
	let configured = $state(true);
	let activeOnly = $state(false);
	let query = $state('');
	let loading = $state(true);
	let error = $state<string | null>(null);
	let timer: ReturnType<typeof setInterval> | undefined;

	async function load(): Promise<void> {
		error = null;
		try {
			const result = await graphql<
				ExecutorsResult,
				{ query: string | null; active: boolean | null; first: number }
			>(EXECUTORS, { query: query.trim() || null, active: activeOnly || null, first: 200 });
			configured = result.areExecutorsConfigured;
			executors = result.executors.nodes;
			totalCount = result.executors.totalCount;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loading = false;
		}
	}

	const COMPATIBILITY: Record<string, string> = {
		OUTDATED: 'outdated: upgrade the executor',
		TOO_NEW: 'newer than the server: upgrade Sourcegull'
	};

	function ago(iso: string): string {
		const secs = Math.max(0, Math.round((Date.now() - new Date(iso).getTime()) / 1000));
		if (secs < 60) return `${secs}s ago`;
		if (secs < 3600) return `${Math.round(secs / 60)}m ago`;
		return `${Math.round(secs / 3600)}h ago`;
	}

	onMount(() => {
		void load();
		timer = setInterval(() => void load(), REFRESH_MS);
	});
	onDestroy(() => clearInterval(timer));
</script>

<svelte:head><title>Executors · Site admin · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Executors</h1>
		<p class="page-description">
			Executors run auto-indexing jobs in containers. They connect to this instance with the token
			set in <code class="code-inline">executors.accessToken</code>.
		</p>
		<RelatedLinks />
	</div>
	<div class="page-actions">
		<span class="badge">{totalCount} seen in the last day</span>
	</div>
</div>

{#if !configured}
	<div class="alert alert-warning" role="status">
		Executors cannot connect: set <code>executors.accessToken</code> (20 characters or more) in the
		<a href="/site-admin/configuration">site configuration</a>, and give executors the same value
		as <code>SOURCEGULL_EXECUTOR_TOKEN</code>.
	</div>
{/if}
{#if error}
	<div class="alert alert-danger" role="alert">{error}</div>
{/if}

<form
	class="toolbar filters"
	onsubmit={(e) => {
		e.preventDefault();
		void load();
	}}
>
	<input
		class="input"
		type="search"
		placeholder="Filter by hostname or queue"
		aria-label="Filter executors"
		bind:value={query}
	/>
	<label class="check">
		<input type="checkbox" bind:checked={activeOnly} onchange={() => void load()} />
		Active only
	</label>
</form>

<div class="table-scroll">
	<table class="table table-bordered" aria-busy={loading}>
		<thead>
			<tr>
				<th scope="col">Executor</th>
				<th scope="col">Queues</th>
				<th scope="col">Running jobs</th>
				<th scope="col">Versions</th>
				<th scope="col">Last seen</th>
			</tr>
		</thead>
		<tbody>
			{#each executors as executor (executor.id)}
				<tr>
					<td>
						<span
							class="dot"
							class:on={executor.active}
							title={executor.active ? 'Active' : 'Inactive'}
						></span>
						<span class="visually-hidden">{executor.active ? 'Active' : 'Inactive'}:</span>
						<strong class="mono">{executor.hostname}</strong>
						<div class="muted small">{executor.os}/{executor.architecture}</div>
					</td>
					<td>{executor.queueNames.join(', ')}</td>
					<td>
						{#if executor.activeJobs.length === 0}
							<span class="muted">none</span>
						{:else}
							{#each executor.activeJobs as job (job.queue + job.id)}
								<span class="badge">{job.queue} #{job.id}</span>
							{/each}
						{/if}
					</td>
					<td class="small">
						<div>
							Executor {executor.executorVersion}
							{#if executor.compatibility && COMPATIBILITY[executor.compatibility]}
								<span class="badge badge-warning">{COMPATIBILITY[executor.compatibility]}</span>
							{/if}
						</div>
						<div class="muted">
							{executor.runtime}
							{executor.runtimeVersion} · git {executor.gitVersion}
						</div>
					</td>
					<td class="nowrap" title={new Date(executor.lastSeenAt).toLocaleString()}>{ago(executor.lastSeenAt)}</td>
				</tr>
			{:else}
				<tr>
					<td colspan="5" class="empty-cell"
						>{loading ? 'Loading…' : 'No executors have connected in the last day.'}</td
					>
				</tr>
			{/each}
		</tbody>
	</table>
</div>

<style>
	.filters {
		gap: var(--space-3);
		margin-bottom: var(--space-4);
	}
	.filters input[type='search'] {
		flex: 1 1 14rem;
		max-width: 20rem;
	}
	.nowrap {
		white-space: nowrap;
	}
	.check {
		display: inline-flex;
		gap: var(--space-2);
		align-items: center;
	}
	.dot {
		display: inline-block;
		width: 0.5rem;
		height: 0.5rem;
		margin-right: var(--space-2);
		border-radius: 50%;
		background: var(--color-border-strong);
		vertical-align: middle;
	}
	.dot.on {
		background: var(--color-success);
	}
	.small {
		font-size: var(--text-sm);
	}
	.badge + .badge {
		margin-left: var(--space-1);
	}
</style>
