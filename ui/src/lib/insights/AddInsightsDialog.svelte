<!--
	"Add or remove trends": every view the viewer sees, checked when it is on the dashboard.
	Saving adds and removes the difference.
-->
<script lang="ts">
	import { onMount } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import { INSIGHT_TYPE_LABELS, insightType, viewChanges, withDashboardParam } from './insights.ts';
	import {
		ADD_INSIGHT_VIEW_TO_DASHBOARD,
		INSIGHT_SUGGESTIONS,
		REMOVE_INSIGHT_VIEW_FROM_DASHBOARD,
		type DashboardNode,
		type DashboardViewVariables,
		type InsightSuggestion,
		type InsightSuggestionsResult
	} from './queries.ts';

	interface Props {
		dashboard: DashboardNode;
		/** The ids of the views on the dashboard now. */
		viewIds: string[];
		onclose: () => void;
		onsaved: () => void;
	}

	let { dashboard, viewIds, onclose, onsaved }: Props = $props();

	const PAGE_SIZE = 20;

	let dialog: HTMLDialogElement;
	let find = $state('');
	let items = $state.raw<InsightSuggestion[]>([]);
	let totalCount = $state<number | null>(null);
	let cursor = $state<string | null>(null);
	let hasMore = $state(false);
	let loading = $state(false);
	let error = $state<string | null>(null);
	// svelte-ignore state_referenced_locally
	let selected = $state<string[]>([...viewIds]);
	let saving = $state(false);
	let seq = 0;

	async function load(after: string | null): Promise<void> {
		const mine = ++seq;
		loading = true;
		error = null;
		try {
			const r = await graphql<InsightSuggestionsResult, { first: number; after: string | null; find: string | null }>(
				INSIGHT_SUGGESTIONS,
				{ first: PAGE_SIZE, after, find: find.trim() || null }
			);
			if (mine !== seq) return;
			const nodes = r.insightViews.nodes.filter((n): n is InsightSuggestion => n !== null);
			items = after ? [...items, ...nodes] : nodes;
			totalCount = r.insightViews.totalCount;
			cursor = r.insightViews.pageInfo.endCursor;
			hasMore = r.insightViews.pageInfo.hasNextPage;
		} catch (err) {
			if (mine === seq) error = errorMessage(err);
		} finally {
			if (mine === seq) loading = false;
		}
	}

	let debounce: ReturnType<typeof setTimeout> | undefined;
	function oninput(): void {
		clearTimeout(debounce);
		debounce = setTimeout(() => load(null), 200);
	}

	onMount(() => {
		dialog.showModal();
		void load(null);
		return () => clearTimeout(debounce);
	});

	function toggle(id: string, on: boolean): void {
		selected = on ? [...selected, id] : selected.filter((s) => s !== id);
	}

	function details(item: InsightSuggestion): string {
		const type = insightType(item);
		const queries = item.dataSeriesDefinitions.map((d) => d.query).filter(Boolean);
		if (type === 'language-stats') return '';
		if (type === 'compute') {
			const g = item.dataSeriesDefinitions[0]?.groupBy ?? '';
			return `${queries[0] ?? ''}, grouped by ${g.charAt(0)}${g.slice(1).toLowerCase()}`;
		}
		return queries.join(', ');
	}

	async function save(e: SubmitEvent): Promise<void> {
		e.preventDefault();
		saving = true;
		error = null;
		try {
			const { add, remove } = viewChanges(viewIds, selected);
			for (const insightViewId of add) {
				await graphql<unknown, DashboardViewVariables>(ADD_INSIGHT_VIEW_TO_DASHBOARD, {
					insightViewId,
					dashboardId: dashboard.id
				});
			}
			for (const insightViewId of remove) {
				await graphql<unknown, DashboardViewVariables>(REMOVE_INSIGHT_VIEW_FROM_DASHBOARD, {
					insightViewId,
					dashboardId: dashboard.id
				});
			}
			onsaved();
		} catch (err) {
			error = errorMessage(err);
		} finally {
			saving = false;
		}
	}
</script>

<dialog
	bind:this={dialog}
	class="card"
	aria-label="Add trends to dashboard"
	oncancel={(e) => {
		e.preventDefault();
		onclose();
	}}
>
	<form onsubmit={save}>
		<header>
			<h2>Add trend to <q>{dashboard.title}</q></h2>
			<button type="button" class="btn btn-sm" aria-label="Close" onclick={onclose}>✕</button>
		</header>
		<label class="visually-hidden" for="insight-find">Search trends</label>
		<input
			id="insight-find"
			class="input"
			type="search"
			placeholder="Search trends…"
			autocomplete="off"
			bind:value={find}
			{oninput}
		/>
		<small class="description">
			<span>
				Don't see a trend? <a href={withDashboardParam('/trends/create', dashboard.id)}>Create a new trend.</a>
			</span>
			{#if totalCount !== null}
				<span class="muted">{items.length} of {totalCount} {totalCount === 1 ? 'trend' : 'trends'}</span>
			{/if}
		</small>
		<ul class="list" aria-busy={loading} aria-label="Trends">
			{#each items as item (item.id)}
				<li>
					<label>
						<input
							type="checkbox"
							checked={selected.includes(item.id)}
							onchange={(e) => toggle(item.id, e.currentTarget.checked)}
						/>
						<span class="text">
							<span class="title">{item.presentation.title}</span>
							<small class="muted">{INSIGHT_TYPE_LABELS[insightType(item)]} trend {details(item)}</small>
						</span>
					</label>
				</li>
			{:else}
				<li class="muted empty">{loading ? 'Loading…' : 'No trends found'}</li>
			{/each}
		</ul>
		{#if hasMore}
			<button type="button" class="btn btn-sm more" disabled={loading} onclick={() => load(cursor)}
				>Load more trends</button
			>
		{/if}
		{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}
		<footer>
			<button type="button" class="btn" onclick={onclose}>Cancel</button>
			<button type="submit" class="btn btn-primary" disabled={saving}>{saving ? 'Saving' : 'Save'}</button>
		</footer>
	</form>
</dialog>

<style>
	dialog {
		width: min(36rem, calc(100vw - 2rem));
		max-height: calc(100vh - 4rem);
		padding: var(--space-4);
		color: var(--color-text);
	}
	dialog::backdrop {
		background: var(--overlay);
	}
	form {
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
	}
	header {
		display: flex;
		align-items: flex-start;
		justify-content: space-between;
		gap: var(--space-2);
	}
	h2 {
		margin: 0;
		font-size: 1.2rem;
		font-weight: 500;
		overflow-wrap: anywhere;
	}
	.description {
		display: flex;
		flex-wrap: wrap;
		justify-content: space-between;
		gap: var(--space-2);
	}
	.list {
		list-style: none;
		margin: 0;
		padding: 0;
		max-height: 22rem;
		overflow-y: auto;
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
	}
	.list li {
		border-bottom: 1px solid var(--color-border);
	}
	.list li:last-child {
		border-bottom: 0;
	}
	.list label {
		display: flex;
		align-items: flex-start;
		gap: var(--space-2);
		padding: var(--space-2);
		cursor: pointer;
	}
	.list label:hover {
		background: var(--color-bg-muted);
	}
	.text {
		display: flex;
		flex-direction: column;
		min-width: 0;
	}
	.text small {
		overflow-wrap: anywhere;
	}
	.title {
		font-weight: 600;
	}
	.empty {
		padding: var(--space-3);
		text-align: center;
	}
	.more {
		align-self: center;
	}
	footer {
		display: flex;
		justify-content: flex-end;
		gap: var(--space-2);
		margin-top: var(--space-2);
	}
</style>
