<!--
	One trend view on a dashboard: title, drill-down filters, the context menu (with "Export
	data" and the "Start Y Axis at 0" chart setting), and the chart (InsightCardBody). The card
	loads its own data, with the filters applied in it, once it scrolls into view, and polls
	while the series backfill.
-->
<script lang="ts">
	import type { SaveInsightAsNewViewMutation } from '#lib/graphql-types.ts';
	import { onMount } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import ExportDataDialog from './ExportDataDialog.svelte';
	import FiltersPanel from './FiltersPanel.svelte';
	import InsightCardBody from './InsightCardBody.svelte';
	import {
		filtersEqual,
		filtersOf,
		hasActiveFilters,
		hasFilters,
		hasNumSamples,
		insightURL,
		isLoadingData,
		toDisplayOptionsInput,
		toFiltersInput,
		updateFiltersInput,
		withDashboardParam,
		type InsightFilters
	} from './insights.ts';
	import { hasExport, hasYAxisOption, incompleteAlertMessage, topLevelIncompleteAlert } from './charts.ts';
	import {
		DELETE_INSIGHT_VIEW,
		INSIGHT_VIEW,
		REMOVE_INSIGHT_VIEW_FROM_DASHBOARD,
		SAVE_INSIGHT_AS_NEW_VIEW,
		UPDATE_LINE_CHART_SEARCH_INSIGHT,
		type DashboardNode,
		type DashboardViewVariables,
		type InsightViewNode,
		type InsightViewResult,
		type InsightViewVariables,
		type SaveInsightAsNewViewInput,
		type UpdateLineChartSearchInsightInput
	} from './queries.ts';

	interface Props {
		id: string;
		/** The dashboard the card is on; null on "All trends" and the trend's own page. */
		dashboard: DashboardNode | null;
		/** On the "All trends" tab: "Remove from this dashboard" is shown disabled. */
		virtualDashboard?: boolean;
		/** Delay before changed filters are applied (ms). */
		debounce?: number;
		onremoved?: (id: string) => void;
		ondeleted?: (id: string) => void;
		/** A view was saved from this one ("Save as new view"). */
		onviewcreated?: (id: string) => void;
	}

	let {
		id,
		dashboard,
		virtualDashboard = false,
		debounce = 500,
		onremoved,
		ondeleted,
		onviewcreated
	}: Props = $props();

	const POLL_INTERVAL = 5000;

	let view = $state.raw<InsightViewNode | null>(null);
	let loading = $state(false);
	let error = $state<string | null>(null);
	let notFound = $state(false);
	/** The filters saved on the view, and those applied in the card. */
	let original = $state.raw<InsightFilters | null>(null);
	let filters = $state.raw<InsightFilters | null>(null);
	let filtersOpen = $state(false);
	let menuOpen = $state(false);
	let actionError = $state<string | null>(null);
	let copied = $state(false);
	let busy = $state(false);
	let zeroYAxisMin = $state(false);
	let exportOpen = $state(false);

	let root: HTMLElement;
	let visible = false;
	let seq = 0;
	let debounceTimer: ReturnType<typeof setTimeout> | undefined;
	let pollTimer: ReturnType<typeof setTimeout> | undefined;

	const title = $derived(view?.presentation.title ?? '');
	const filtered = $derived(filters !== null && hasActiveFilters(filters));
	const incompleteAlert = $derived(view ? topLevelIncompleteAlert(view) : null);

	function variables(f: InsightFilters | null): InsightViewVariables {
		if (!f || !original || filtersEqual(f, original)) return { id };
		return { id, filters: toFiltersInput(f), seriesDisplayOptions: toDisplayOptionsInput(f) };
	}

	async function load(f: InsightFilters | null, quiet = false): Promise<void> {
		const mine = ++seq;
		clearTimeout(pollTimer);
		if (!quiet) loading = true;
		try {
			const r = await graphql<InsightViewResult, InsightViewVariables>(INSIGHT_VIEW, variables(f));
			if (mine !== seq) return;
			const node = r.insightViews.nodes[0] ?? null;
			error = null;
			view = node;
			notFound = node === null;
			if (node && original === null) {
				original = filtersOf(node.defaultFilters, node.defaultSeriesDisplayOptions);
				filters = original;
			}
			if (node && isLoadingData(node)) {
				pollTimer = setTimeout(() => {
					if (visible) void load(filters, true);
				}, POLL_INTERVAL);
			}
		} catch (err) {
			if (mine === seq) error = errorMessage(err);
		} finally {
			if (mine === seq) loading = false;
		}
	}

	onMount(() => {
		if (typeof IntersectionObserver === 'undefined') {
			visible = true;
			void load(null);
			return () => clearTimers();
		}
		let started = false;
		const observer = new IntersectionObserver((entries) => {
			const wasVisible = visible;
			visible = entries.some((e) => e.isIntersecting);
			if (visible && !started) {
				started = true;
				void load(null);
			} else if (visible && !wasVisible && view && isLoadingData(view)) {
				void load(filters, true);
			}
		});
		observer.observe(root);
		return () => {
			observer.disconnect();
			clearTimers();
		};
	});

	function clearTimers(): void {
		clearTimeout(debounceTimer);
		clearTimeout(pollTimer);
	}

	function onfilterchange(f: InsightFilters): void {
		filters = f;
		clearTimeout(debounceTimer);
		debounceTimer = setTimeout(() => void load(f), debounce);
	}

	async function onfiltersave(f: InsightFilters): Promise<void> {
		if (!view) return;
		await graphql<unknown, { id: string; input: UpdateLineChartSearchInsightInput }>(
			UPDATE_LINE_CHART_SEARCH_INSIGHT,
			{ id, input: updateFiltersInput(view, f) }
		);
		original = f;
		filters = f;
		filtersOpen = false;
		await load(f);
	}

	async function onviewcreate(viewTitle: string): Promise<void> {
		if (!filters) return;
		const r = await graphql<SaveInsightAsNewViewMutation, { input: SaveInsightAsNewViewInput }>(
			SAVE_INSIGHT_AS_NEW_VIEW,
			{
				input: {
					insightViewId: id,
					options: { title: viewTitle },
					dashboard: dashboard?.id ?? null,
					viewControls: { filters: toFiltersInput(filters), seriesDisplayOptions: toDisplayOptionsInput(filters) }
				}
			}
		);
		// This view keeps its own filters.
		filtersOpen = false;
		filters = original;
		await load(original);
		onviewcreated?.(r.saveInsightAsNewView.view.id);
	}

	async function removeFromDashboard(): Promise<void> {
		menuOpen = false;
		if (!dashboard) return;
		if (!confirm(`Are you sure you want to remove the trend “${title}” from the dashboard “${dashboard.title}”?`)) {
			return;
		}
		busy = true;
		actionError = null;
		try {
			await graphql<unknown, DashboardViewVariables>(REMOVE_INSIGHT_VIEW_FROM_DASHBOARD, {
				insightViewId: id,
				dashboardId: dashboard.id
			});
			onremoved?.(id);
		} catch (err) {
			actionError = errorMessage(err);
		} finally {
			busy = false;
		}
	}

	async function deleteInsight(): Promise<void> {
		menuOpen = false;
		if (!confirm(`Are you sure you want to delete the trend “${title}”? It is removed from every dashboard.`)) {
			return;
		}
		busy = true;
		actionError = null;
		try {
			await graphql<unknown, { id: string }>(DELETE_INSIGHT_VIEW, { id });
			ondeleted?.(id);
		} catch (err) {
			actionError = errorMessage(err);
		} finally {
			busy = false;
		}
	}

	async function copyLink(): Promise<void> {
		menuOpen = false;
		try {
			await navigator.clipboard.writeText(new URL(insightURL(id), location.origin).href);
			copied = true;
			setTimeout(() => (copied = false), 2000);
		} catch (err) {
			actionError = `Copy failed: ${errorMessage(err)}`;
		}
	}

	function onwindowclick(e: MouseEvent): void {
		if (!e.composedPath().includes(root)) {
			menuOpen = false;
			filtersOpen = false;
		}
	}

	function onkeydown(e: KeyboardEvent): void {
		if (e.key === 'Escape' && (menuOpen || filtersOpen)) {
			menuOpen = false;
			filtersOpen = false;
		}
	}
</script>

<svelte:window onclick={onwindowclick} {onkeydown} />

<article
	class="insight-card card"
	class:with-filters={filtersOpen}
	bind:this={root}
	aria-label={title ? `${title} trend` : 'Trend'}
	aria-busy={loading || busy}
	data-testid={`insight-card.${id}`}
>
	<header>
		<div class="heading">
			<h3>
				{#if view}
					<a href={insightURL(id)}>{title}</a>
				{:else}
					<span class="muted">{notFound ? 'Trend not found' : 'Loading trend'}</span>
				{/if}
			</h3>
			{#if view && isLoadingData(view)}
				<small class="muted">Datapoints shown may be undercounted. Processing time may vary depending on the trend’s scope.</small>
			{/if}
		</div>
		{#if view}
			<div class="actions">
				{#if incompleteAlert}
					<span class="badge incomplete" role="img" aria-label="Trend is in incomplete state" title={incompleteAlertMessage(incompleteAlert)}
						>incomplete</span
					>
				{/if}
				{#if copied}<span class="muted copied" role="status">Copied!</span>{/if}
				{#if hasFilters(view)}
					<button
						type="button"
						class="btn btn-sm icon"
						class:active={filtered}
						aria-label={filtered ? 'Active filters' : 'Filters'}
						aria-expanded={filtersOpen}
						onclick={() => {
							filtersOpen = !filtersOpen;
							menuOpen = false;
						}}
					>
						<svg viewBox="0 0 24 24" width="16" height="16" aria-hidden="true"
							><path fill="currentColor" d="M3 5h18l-7 8.5V20l-4-2v-4.5L3 5Zm4.3 2 4.7 5.7V16.8l0 0V12.7L16.7 7H7.3Z" /></svg
						>
					</button>
				{/if}
				<div class="menu-wrap">
					<button
						type="button"
						class="btn btn-sm icon"
						aria-label="Trend options"
						aria-haspopup="menu"
						aria-expanded={menuOpen}
						onclick={() => {
							menuOpen = !menuOpen;
							filtersOpen = false;
						}}>⋮</button
					>
					{#if menuOpen}
						<div class="menu card" role="menu" aria-label="Trend options">
							<div class="menu-heading">Trend</div>
							<a role="menuitem" href={withDashboardParam(`/trends/${encodeURIComponent(id)}/edit`, dashboard?.id)}
								>Edit</a
							>
							<button role="menuitem" type="button" onclick={copyLink}>Get shareable link</button>
							{#if hasExport(view)}
								<button
									role="menuitem"
									type="button"
									onclick={() => {
										menuOpen = false;
										exportOpen = true;
									}}>Export data</button
								>
							{/if}
							<hr />
							{#if hasYAxisOption(view)}
								<div class="menu-heading">Chart settings</div>
								<button
									role="menuitemcheckbox"
									type="button"
									aria-checked={zeroYAxisMin}
									onclick={() => (zeroYAxisMin = !zeroYAxisMin)}
									><input type="checkbox" checked={zeroYAxisMin} tabindex="-1" aria-hidden="true" /> Start Y Axis at 0</button
								>
								<hr />
							{/if}
							<div class="menu-heading">Others</div>
							{#if dashboard || virtualDashboard}
								<button
									role="menuitem"
									type="button"
									disabled={!dashboard}
									title={dashboard ? undefined : "Removing trend isn't available for the All trends dashboard"}
									onclick={removeFromDashboard}>Remove from this dashboard</button
								>
							{/if}
							<button role="menuitem" type="button" class="danger" onclick={deleteInsight}>Delete</button>
						</div>
					{/if}
				</div>
			</div>
		{/if}
	</header>

	{#if filtersOpen && view && filters && original}
		<div class="filters card">
			<FiltersPanel
				value={filters}
				{original}
				numSamples={hasNumSamples(view)}
				onchange={onfilterchange}
				onsave={onfiltersave}
				oncreate={onviewcreate}
			/>
		</div>
	{/if}

	{#if actionError}<div class="alert alert-danger" role="alert">{actionError}</div>{/if}

	{#if error}
		<div class="alert alert-danger" role="alert">{error}</div>
	{:else if notFound}
		<p class="muted">This trend was deleted or you may not see it.</p>
	{:else if !view || loading}
		<p class="muted loading" role="status">Loading trend…</p>
	{:else}
		<InsightCardBody {view} {zeroYAxisMin} />
	{/if}

	{#if exportOpen && view}
		<ExportDataDialog {id} {title} onclose={() => (exportOpen = false)} />
	{/if}
</article>

<style>
	.insight-card {
		position: relative;
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
		padding: var(--space-3) var(--space-4);
		min-height: 18rem;
		min-width: 0;
	}
	.insight-card.with-filters {
		z-index: 5;
	}
	header {
		display: flex;
		align-items: flex-start;
		gap: var(--space-2);
	}
	.heading {
		flex: 1;
		min-width: 0;
	}
	h3 {
		margin: 0;
		font-size: 1rem;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	h3 a {
		color: var(--color-text);
	}
	.actions {
		display: flex;
		align-items: center;
		gap: var(--space-1);
		flex: none;
	}
	.icon {
		padding: 0.15rem 0.4rem;
		line-height: 1;
	}
	.icon.active {
		color: var(--color-primary);
		border-color: var(--color-primary);
		background: var(--color-info-bg);
	}
	.incomplete {
		background: var(--color-warning-bg);
		color: var(--color-warning);
	}
	.copied {
		font-size: 0.85rem;
	}
	.menu-wrap {
		position: relative;
	}
	.menu {
		position: absolute;
		right: 0;
		top: calc(100% + var(--space-1));
		z-index: 20;
		min-width: 14rem;
		padding: var(--space-1) 0;
		display: flex;
		flex-direction: column;
		box-shadow: var(--shadow-lg);
	}
	.menu-heading {
		padding: var(--space-1) var(--space-3);
		font-size: 0.8rem;
		font-weight: 600;
		color: var(--color-text-muted);
	}
	.menu a,
	.menu button {
		display: block;
		padding: var(--space-1) var(--space-3);
		border: 0;
		background: none;
		color: var(--color-text);
		font: inherit;
		text-align: left;
		cursor: pointer;
	}
	.menu a:hover,
	.menu button:hover:not(:disabled) {
		background: var(--color-bg-muted);
		text-decoration: none;
	}
	.menu button:disabled {
		opacity: 0.55;
		cursor: not-allowed;
	}
	.menu input[type='checkbox'] {
		margin: 0 var(--space-1) 0 0;
		pointer-events: none;
		vertical-align: -2px;
	}
	.menu .danger {
		color: var(--color-danger);
	}
	.menu hr {
		border: 0;
		border-top: 1px solid var(--color-border);
		margin: var(--space-1) 0;
	}
	.filters {
		position: absolute;
		top: 2.75rem;
		right: var(--space-2);
		z-index: 15;
		width: min(26rem, calc(100% - 2 * var(--space-2)));
		max-height: 32rem;
		overflow-y: auto;
		padding: var(--space-3);
		box-shadow: var(--shadow-lg);
	}
	.loading {
		margin: auto;
	}
</style>
