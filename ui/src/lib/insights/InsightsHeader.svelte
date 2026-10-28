<script lang="ts">
	import Icon from '#lib/components/Icon.svelte';
	import { withDashboardParam } from './insights.ts';

	interface Props {
		tab: 'dashboards' | 'all' | 'about';
		/** The dashboard in view (or from `?dashboardId=` on the other tabs). */
		dashboardId: string | null;
	}

	let { tab, dashboardId }: Props = $props();

	const dashboardsHref = $derived(
		dashboardId && tab !== 'dashboards' ? `/trends/dashboards/${encodeURIComponent(dashboardId)}` : '/trends/dashboards'
	);
</script>

<div class="page-header">
	<div>
		<h1>Trends</h1>
		<p class="page-description">Charts of how search results change over time, collected on dashboards.</p>
	</div>
	<div class="page-actions">
		<a class="btn" href="/trends/add-dashboard"><Icon name="plus" size={16} />Add dashboard</a>
		<a class="btn btn-primary" href={withDashboardParam('/trends/create', dashboardId)}
			><Icon name="plus" size={16} />Create trend</a
		>
	</div>
</div>

<nav class="tabs" aria-label="Trends">
	<a href={dashboardsHref} aria-current={tab === 'dashboards' ? 'page' : undefined}>Dashboards</a>
	<a href={withDashboardParam('/trends/all', dashboardId)} aria-current={tab === 'all' ? 'page' : undefined}
		>All trends</a
	>
	<a href={withDashboardParam('/trends/about', dashboardId)} aria-current={tab === 'about' ? 'page' : undefined}
		>Getting started</a
	>
</nav>

