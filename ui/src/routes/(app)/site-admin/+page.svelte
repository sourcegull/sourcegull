<script lang="ts">
	import { onMount } from 'svelte';
	import Icon from '#lib/components/Icon.svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import { SITE_ADMIN_NAV } from '#lib/admin/nav.ts';
	import {
		SITE_ADMIN_COUNTS,
		SITE_ADMIN_HEALTH,
		diskUsedPercent,
		type SiteAdminCountsResult,
		type SiteAdminHealthResult
	} from '#lib/admin/overview.ts';
	import { formatBytes } from '#lib/repos-admin/queries.ts';
	import { percent, summarize, type StatusEntry } from '#lib/status-messages.ts';
	import { setupSteps } from '#lib/admin/setup.ts';
	import SetupChecklist from '#lib/components/SetupChecklist.svelte';
	import { SITE_CONFIGURATION, type SiteConfigurationResult } from '#lib/queries.ts';
	import type { PageProps } from './$types';

	let { data }: PageProps = $props();

	let health = $state.raw<SiteAdminHealthResult | null>(null);
	let counts = $state.raw<SiteAdminCountsResult | null>(null);
	let healthError = $state<string | null>(null);
	let countsError = $state<string | null>(null);
	let loading = $state(true);
	/** The site configuration document, for the setup checklist; null until loaded or when unreadable. */
	let siteConfig = $state<string | null>(null);

	async function load(): Promise<void> {
		loading = true;
		const [h, c, cfg] = await Promise.allSettled([
			graphql<SiteAdminHealthResult>(SITE_ADMIN_HEALTH),
			graphql<SiteAdminCountsResult>(SITE_ADMIN_COUNTS),
			graphql<SiteConfigurationResult>(SITE_CONFIGURATION)
		]);
		if (cfg.status === 'fulfilled') siteConfig = cfg.value.site.configuration.effectiveContents;
		if (h.status === 'fulfilled') {
			health = h.value;
			healthError = null;
		} else healthError = errorMessage(h.reason);
		if (c.status === 'fulfilled') {
			counts = c.value;
			countsError = null;
		} else countsError = errorMessage(c.reason);
		loading = false;
	}

	onMount(() => {
		void load();
	});

	const summary = $derived(health ? summarize(health.statusMessages) : null);

	/** Shown until every step is done or the admin hides it (remembered in this browser). */
	const HIDE_SETUP_KEY = 'sourcegull.siteAdmin.hideSetup';
	let setupHidden = $state(readHidden());
	function readHidden(): boolean {
		try {
			return localStorage.getItem(HIDE_SETUP_KEY) === '1';
		} catch {
			return false;
		}
	}
	function hideSetup(): void {
		setupHidden = true;
		try {
			localStorage.setItem(HIDE_SETUP_KEY, '1');
		} catch {
			// Hidden for this visit only.
		}
	}
	const setup = $derived(
		counts && siteConfig !== null
			? setupSteps({
					siteConfig,
					connections: counts.externalServices.totalCount,
					users: counts.users.totalCount,
					emailEnabled: data.auth.emailEnabled ?? false
				})
			: []
	);
	const showSetup = $derived(!setupHidden && setup.some((s) => !s.done));
	const n = (v: number | undefined) => (v === undefined ? '—' : v.toLocaleString('en-US'));

	const ALERT: Record<StatusEntry['kind'], string> = {
		error: 'alert-danger',
		warning: 'alert-warning',
		progress: 'alert-info',
		info: 'alert-info'
	};

	interface Tile {
		label: string;
		value: string;
		detail?: string;
		href: string;
		tone?: 'danger' | 'warning';
	}

	const tiles = $derived.by<Tile[]>(() => {
		const s = health?.repositoryStats;
		const nodes = health?.gitservers.nodes ?? [];
		const pending = counts?.accessRequestsCount;
		return [
			{
				label: 'Repositories',
				value: n(s?.total),
				detail: s ? `${n(s.cloned)} cloned · ${n(s.indexed)} indexed` : undefined,
				href: '/site-admin/repositories',
				tone: s && (s.failedFetch > 0 || s.corrupted > 0) ? 'danger' : undefined
			},
			{
				label: 'Code host connections',
				value: n(counts?.externalServices.totalCount),
				detail: 'GitLab, GitHub and Gitea',
				href: '/site-admin/external-services'
			},
			{
				label: 'Users',
				value: n(counts?.users.totalCount),
				detail:
					pending !== undefined && pending > 0
						? `${n(pending)} pending access ${pending === 1 ? 'request' : 'requests'}`
						: `${n(counts?.organizations.totalCount)} organizations`,
				href: pending ? '/site-admin/access-requests' : '/site-admin/users',
				tone: pending ? 'warning' : undefined
			},
			{
				label: 'Repo nodes',
				value: n(health?.gitservers.totalCount),
				detail: s ? `${formatBytes(s.gitDirBytes)} of repositories` : undefined,
				href: '/site-admin/repo-nodes',
				tone: nodes.some((x) => x.clonesPaused || x.error)
					? 'danger'
					: undefined
			}
		];
	});
</script>

<svelte:head><title>Overview · Site admin · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Site admin</h1>
		<p class="page-description">
			Health and usage of this Sourcegull instance. Anything that needs your attention is listed first;
			every admin area is linked below.
		</p>
	</div>
	<div class="page-actions">
		{#if health}
			<a class="badge" href="/site-admin/updates" title="Product version">v{health.site.productVersion}</a>
		{/if}
		<a class="btn btn-secondary" href="/site-admin/configuration">
			<Icon name="settings" size={16} />Site configuration
		</a>
	</div>
</div>

{#if healthError}
	<div class="alert alert-danger" role="alert">Could not load the instance status: {healthError}</div>
{/if}

{#if showSetup}
	<div class="block">
		<SetupChecklist title="Set up Sourcegull" titleId="setup-title" steps={setup}>
			{#snippet actions()}
				<button class="btn btn-link btn-sm" type="button" onclick={hideSetup}>Hide</button>
			{/snippet}
		</SetupChecklist>
	</div>
{/if}

<section aria-labelledby="health-title" class="block">
	<h2 id="health-title" class="visually-hidden">Status</h2>
	{#if loading && !summary}
		<div class="loading"><span class="spinner"></span>Checking the instance…</div>
	{:else if summary && summary.entries.length > 0}
		<ul class="status-list" role="list">
			{#each summary.entries as entry, i (i)}
				<li class="alert {ALERT[entry.kind]}">
					<div class="status-body">
						<strong>{entry.title}</strong>
						{#if entry.text}<span class="status-text">{entry.text}</span>{/if}
					</div>
					<a class="btn btn-sm btn-secondary" href={entry.href}>{entry.linkText}</a>
				</li>
			{/each}
		</ul>
	{:else if summary}
		<div class="alert alert-success" role="status">
			<strong>All systems normal.</strong> Repositories are cloned and indexed, and repo nodes have
			disk space.
		</div>
	{/if}
</section>

<section class="tiles block" aria-label="Counts">
	{#each tiles as tile (tile.label)}
		<a class="tile card card-interactive" class:danger={tile.tone === 'danger'} class:warning={tile.tone === 'warning'} href={tile.href}>
			<span class="tile-label">{tile.label}</span>
			<span class="tile-value">{loading && tile.value === '—' ? '…' : tile.value}</span>
			{#if tile.detail}<span class="tile-detail">{tile.detail}</span>{/if}
		</a>
	{/each}
</section>
{#if countsError}
	<p class="error-text small block">Some counts could not be loaded: {countsError}</p>
{/if}

{#if health}
	{@const s = health.repositoryStats}
	<div class="columns block">
		<section class="card">
			<div class="card-header">
				<h2>Repository sync</h2>
				<a class="small" href="/site-admin/repositories">All repositories</a>
			</div>
			{#if s.total === 0}
				<p class="muted">
					No repositories yet. <a href="/site-admin/external-services">Add a code host connection</a> to
					start cloning.
				</p>
			{:else}
				<div class="progress-row">
					<span>Cloned</span>
					<progress max="100" value={percent(s.cloned, s.total)} aria-label="Cloned"></progress>
					<span class="num">{n(s.cloned)} / {n(s.total)}</span>
				</div>
				<div class="progress-row">
					<span>Indexed</span>
					<progress max="100" value={percent(s.indexed, s.cloned)} aria-label="Indexed"></progress>
					<span class="num">{n(s.indexed)} / {n(s.cloned)}</span>
				</div>
				<dl class="facts">
					<div><dt>Cloning</dt><dd class="num">{n(s.cloning)}</dd></div>
					<div><dt>Queued</dt><dd class="num">{n(s.queued)}</dd></div>
					<div>
						<dt>Failed</dt>
						<dd class="num">
							{#if s.failedFetch > 0}<a class="error-text" href="/site-admin/repositories?filter=failed"
									>{n(s.failedFetch)}</a
								>{:else}0{/if}
						</dd>
					</div>
					<div>
						<dt>Corrupted</dt>
						<dd class="num">
							{#if s.corrupted > 0}<a class="error-text" href="/site-admin/repositories?filter=corrupted"
									>{n(s.corrupted)}</a
								>{:else}0{/if}
						</dd>
					</div>
				</dl>
			{/if}
		</section>

		<section class="card">
			<div class="card-header">
				<h2>Repo nodes</h2>
				<a class="small" href="/site-admin/repo-nodes">Details</a>
			</div>
			{#if health.gitservers.nodes.length === 0}
				<p class="muted">No repo nodes have reported in yet.</p>
			{:else}
				<ul class="nodes" role="list">
					{#each health.gitservers.nodes as node (node.id)}
						{@const used = diskUsedPercent(node)}
						<li>
							<div class="node-head">
								<span class="mono">{node.address}</span>
								{#if node.error}
									<span class="badge badge-danger" title={node.error}>unreachable</span>
								{:else if node.clonesPaused}
									<span class="badge badge-danger">clones paused</span>
								{:else if used !== null && used >= 90}
									<span class="badge badge-warning">low disk space</span>
								{:else}
									<span class="badge badge-success">ok</span>
								{/if}
							</div>
							{#if used !== null}
								<progress
									class="disk"
									class:high={used >= 90}
									max="100"
									value={used}
									aria-label="Disk used on {node.address}"
								></progress>
								<span class="muted small">
									{used}% used · {formatBytes(node.freeDiskSpaceBytes)} free of
									{formatBytes(node.totalDiskSpaceBytes)} · {n(node.repositoriesCount)} repositories
								</span>
							{:else}
								<span class="muted small">Disk size unknown · {n(node.repositoriesCount)} repositories</span>
							{/if}
						</li>
					{/each}
				</ul>
			{/if}
		</section>
	</div>
{/if}

<div class="section-header">
	<h2>Administration</h2>
</div>
<div class="sections">
	{#each SITE_ADMIN_NAV.filter((s) => s.title !== 'Overview') as section (section.title)}
		<section class="card section-card" aria-label={section.title}>
			<h3>{section.title}</h3>
			<p class="muted small">{section.description}</p>
			<ul role="list">
				{#each section.items as item (item.href)}
					<li><a href={item.href}>{item.title}</a></li>
				{/each}
			</ul>
		</section>
	{/each}
</div>

<style>
	.block {
		margin-bottom: var(--space-4);
	}
	.status-list {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: var(--space-2);
	}
	.status-list .alert {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--space-3);
		flex-wrap: wrap;
		margin: 0;
	}
	.status-body {
		display: flex;
		flex-direction: column;
		gap: var(--space-1);
		min-width: 0;
		flex: 1 1 20rem;
	}
	.status-text {
		color: var(--color-text-muted);
	}
	.tiles {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(12rem, 1fr));
		gap: var(--space-3);
	}
	.tile {
		display: flex;
		flex-direction: column;
		gap: var(--space-1);
		padding: var(--space-4) var(--space-5);
		color: var(--color-text);
		position: relative;
	}
	.tile:hover {
		text-decoration: none;
	}
	.tile-label {
		font-size: var(--text-sm);
		font-weight: var(--weight-medium);
		color: var(--color-text-muted);
	}
	.tile-value {
		font-size: var(--text-2xl);
		font-weight: var(--weight-semibold);
		font-variant-numeric: tabular-nums;
		line-height: 1.2;
	}
	.tile-detail {
		font-size: var(--text-xs);
		color: var(--color-text-muted);
	}
	.tile.danger {
		border-left: 3px solid var(--color-danger);
	}
	.tile.warning {
		border-left: 3px solid var(--color-warning);
	}
	.columns {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(20rem, 1fr));
		gap: var(--space-4);
		margin-top: var(--space-6);
	}
	.card-header a {
		font-weight: var(--weight-medium);
	}
	.progress-row {
		display: grid;
		grid-template-columns: 4.5rem minmax(0, 1fr) auto;
		align-items: center;
		gap: var(--space-3);
		margin-bottom: var(--space-2);
	}
	.progress-row progress,
	progress.disk {
		width: 100%;
		height: 0.5rem;
		accent-color: var(--color-primary);
	}
	progress.disk.high {
		accent-color: var(--color-danger);
	}
	.num {
		font-variant-numeric: tabular-nums;
		text-align: right;
	}
	.facts {
		display: grid;
		grid-template-columns: repeat(4, minmax(0, 1fr));
		gap: var(--space-2);
		margin: var(--space-4) 0 0;
		padding-top: var(--space-3);
		border-top: 1px solid var(--color-border);
	}
	.facts div {
		display: flex;
		flex-direction: column;
	}
	.facts dt {
		font-size: var(--text-xs);
		color: var(--color-text-muted);
	}
	.facts dd {
		margin: 0;
		font-weight: var(--weight-semibold);
		text-align: left;
	}
	.nodes {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: var(--space-3);
	}
	.nodes li {
		display: flex;
		flex-direction: column;
		gap: var(--space-1);
	}
	.node-head {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--space-2);
		min-width: 0;
	}
	.node-head .mono {
		overflow: hidden;
		text-overflow: ellipsis;
	}
	.sections {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(15rem, 1fr));
		gap: var(--space-3);
	}
	.section-card {
		padding: var(--space-4) var(--space-5);
	}
	.section-card h3 {
		margin: 0 0 var(--space-1);
		font-size: var(--text-md);
	}
	.section-card p {
		margin: 0 0 var(--space-3);
	}
	.section-card ul {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: var(--space-1);
		font-size: var(--text-sm);
	}
	@media (max-width: 40rem) {
		.facts {
			grid-template-columns: repeat(2, minmax(0, 1fr));
		}
	}
</style>
