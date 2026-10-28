<script lang="ts">
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import { describeTarget, formatHours, RETENTION_OVERVIEW, type RetentionResult } from './policies.ts';

	interface Props {
		/** The precise index's node ID. */
		indexId: string;
	}

	let { indexId }: Props = $props();

	let result = $state.raw<RetentionResult | null>(null);
	let error = $state<string | null>(null);

	$effect(() => {
		const id = indexId;
		result = null;
		error = null;
		graphql<RetentionResult, { id: string }>(RETENTION_OVERVIEW, { id })
			.then((r) => {
				if (id === indexId) result = r;
			})
			.catch((err) => {
				if (id === indexId) error = errorMessage(err);
			});
	});

	const matches = $derived(result?.node?.retentionPolicyOverview?.nodes.filter((n) => n.matches) ?? []);
	const others = $derived(result?.node?.retentionPolicyOverview?.nodes.filter((n) => !n.matches) ?? []);
</script>

<div class="retention" aria-busy={!result && !error}>
	{#if error}
		<span class="text-danger">{error}</span>
	{:else if !result}
		<span class="muted">Checking retention…</span>
	{:else}
		{#if result.node?.expired}
			<p>
				<strong>Expired:</strong> no policy keeps this index.
				{#if (result.dependents.totalCount ?? 0) > 0}
					It stays while other indexes use packages it defines.
				{:else}
					It will be deleted.
				{/if}
			</p>
		{/if}
		{#if matches.length}
			<p class="muted">Kept by:</p>
			<ul>
				{#each matches as m, i (m.configurationPolicy?.id ?? `tip-${i}`)}
					<li>
						{#if m.configurationPolicy}
							<strong>{m.configurationPolicy.name}</strong>
							<span class="muted">
								({describeTarget(m.configurationPolicy.type, m.configurationPolicy.pattern)},
								{formatHours(m.configurationPolicy.retentionDurationHours)})
							</span>
						{:else}
							<strong>Tip of the default branch</strong> <span class="muted">(always)</span>
						{/if}
						{#if m.protectingCommits?.length}
							<span class="muted">
								— serves {m.protectingCommits.map((c) => c.slice(0, 7)).join(', ')}
							</span>
						{/if}
					</li>
				{/each}
			</ul>
		{:else if !result.node?.expired}
			<p class="muted">No policy keeps this index any more; it expires at the next retention scan.</p>
		{/if}
		{#if (result.dependents.totalCount ?? 0) > 0}
			<p class="muted">
				Used by {result.dependents.totalCount}
				{result.dependents.totalCount === 1 ? 'index' : 'indexes'}:
				{#each result.dependents.nodes as d, i (d.id)}
					{#if i > 0},{/if}
					{#if d.projectRoot}<a href={d.projectRoot.url}>{d.projectRoot.repository.name}{d.inputRoot ? `/${d.inputRoot}` : ''}</a>{:else}{d.inputRoot || '/'}{/if}
				{/each}
			</p>
		{/if}
		{#if others.length}
			<details>
				<summary class="muted">Policies that don't keep it ({others.length})</summary>
				<ul>
					{#each others as m, i (m.configurationPolicy?.id ?? `other-${i}`)}
						{#if m.configurationPolicy}
							<li>
								{m.configurationPolicy.name}
								<span class="muted">
									({describeTarget(m.configurationPolicy.type, m.configurationPolicy.pattern)},
									{formatHours(m.configurationPolicy.retentionDurationHours)})
								</span>
							</li>
						{/if}
					{/each}
				</ul>
			</details>
		{/if}
	{/if}
</div>

<style>
	.retention {
		font-size: 0.9em;
	}
	.retention p {
		margin: var(--space-1) 0;
	}
	ul {
		margin: 0 0 var(--space-1);
		padding-left: var(--space-4);
	}
	.text-danger {
		color: var(--color-danger);
	}
</style>
