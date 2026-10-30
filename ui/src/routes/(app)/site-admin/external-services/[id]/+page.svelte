<!--
	A code host connection: status, test connection, sync now, the webhook URL and
	secret (with regeneration), the sync history with per-run counts and errors, and delete. A
	connection the resources file owns keeps testing, syncing and its history; editing, deleting and
	regenerating the secret are left to the file.
-->
<script lang="ts">
	import RelatedLinks from '#lib/components/RelatedLinks.svelte';
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { onDestroy, onMount } from 'svelte';
	import { formatDate, relativeTime } from '#lib/batches/batch-change.ts';
	import ManagedNotice from '#lib/components/ManagedNotice.svelte';
	import {
		CANCEL_EXTERNAL_SERVICE_SYNC,
		CHECK_CONNECTION,
		DELETE_EXTERNAL_SERVICE,
		EXTERNAL_SERVICE,
		REGENERATE_WEBHOOK_SECRET,
		SYNC_EXTERNAL_SERVICE,
		SYNC_STATE_LABELS,
		duration,
		isActive,
		isCancelable,
		kindLabel,
		rateLimitSummary,
		reasonLabel,
		takeFlash,
		type Availability,
		type CheckConnectionResult,
		type ExternalServiceDetail,
		type ExternalServiceResult,
		type RegenerateWebhookSecretResult,
		type SyncJob
	} from '#lib/connections/queries.ts';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import { connectionNextSteps } from '#lib/admin/setup.ts';
	import SetupChecklist from '#lib/components/SetupChecklist.svelte';
	import { SITE_CONFIGURATION, type SiteConfigurationResult } from '#lib/queries.ts';

	const JOBS_PAGE = 20;
	const POLL_MS = 3000;

	const id = $derived(page.params.id ?? '');
	const editURL = $derived(`/site-admin/external-services/${encodeURIComponent(id)}/edit`);

	let service = $state.raw<ExternalServiceDetail | null>(null);
	const host = $derived(service ? kindLabel(service.kind) : 'the code host');
	let jobs = $state.raw<SyncJob[]>([]);
	let jobsTotal = $state(0);
	let jobsShown = $state(JOBS_PAGE);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let actionError = $state<string | null>(null);
	let notice = $state<string | null>(takeFlash());
	let busy = $state(false);

	let checking = $state(false);
	let availability = $state.raw<Availability | null>(null);

	/** The secret `regenerate` returned; shown until the page is left. */
	let newSecret = $state<string | null>(null);
	let copied = $state<string | null>(null);

	let timer: ReturnType<typeof setTimeout> | undefined;

	/** The site configuration, to tell whether people can sign in with this connection's host. */
	let siteConfig = $state<string | null>(null);
	onMount(async () => {
		try {
			siteConfig = (await graphql<SiteConfigurationResult>(SITE_CONFIGURATION)).site.configuration.effectiveContents;
		} catch {
			// Without it the next steps are not shown.
		}
	});
	const nextSteps = $derived(
		service && siteConfig !== null
			? connectionNextSteps(siteConfig, service).map((s) =>
					s.id === 'permissions' ? { ...s, href: service?.managedBy ? '' : editURL } : s
				)
			: []
	);

	async function load(nodeId: string, quiet = false): Promise<void> {
		if (!quiet) loading = true;
		try {
			const r = await graphql<ExternalServiceResult, { id: string; first: number }>(EXTERNAL_SERVICE, {
				id: nodeId,
				first: jobsShown
			});
			if (r.node?.__typename !== 'ExternalService' || !('syncJobs' in r.node)) {
				service = null;
				error = 'Code host connection not found.';
				return;
			}
			service = r.node;
			jobs = r.node.syncJobs.nodes;
			jobsTotal = r.node.syncJobs.totalCount;
			error = null;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loading = false;
			schedulePoll(nodeId);
		}
	}

	/** Reloads every few seconds while a sync is queued or running. */
	function schedulePoll(nodeId: string): void {
		clearTimeout(timer);
		if (jobs.some((j) => isActive(j.state))) {
			timer = setTimeout(() => void load(nodeId, true), POLL_MS);
		}
	}

	onDestroy(() => clearTimeout(timer));

	$effect(() => {
		void load(id);
	});

	async function act(f: () => Promise<unknown>, done?: string): Promise<void> {
		busy = true;
		actionError = null;
		notice = null;
		try {
			await f();
			if (done) notice = done;
			await load(id, true);
		} catch (err) {
			actionError = errorMessage(err);
		} finally {
			busy = false;
		}
	}

	const syncNow = () =>
		act(() => graphql(SYNC_EXTERNAL_SERVICE, { id }), 'Sync queued. The history below updates as it runs.');

	const cancelJob = (job: SyncJob) =>
		act(() => graphql(CANCEL_EXTERNAL_SERVICE_SYNC, { id: job.id }), 'Cancellation requested.');

	async function testConnection(): Promise<void> {
		checking = true;
		availability = null;
		actionError = null;
		try {
			const r = await graphql<CheckConnectionResult, { id: string }>(CHECK_CONNECTION, { id });
			availability = r.node?.__typename === 'ExternalService' ? r.node.checkConnection : null;
		} catch (err) {
			actionError = errorMessage(err);
		} finally {
			checking = false;
		}
	}

	function regenerate(): void {
		if (
			service?.webhookSecret &&
			!confirm(
				`Generate a new webhook secret? Webhooks ${host} sends with the current secret are rejected until you update the webhook in ${host}.`
			)
		)
			return;
		void act(async () => {
			const r = await graphql<RegenerateWebhookSecretResult, { id: string }>(REGENERATE_WEBHOOK_SECRET, { id });
			newSecret = r.regenerateExternalServiceWebhookSecret.webhookSecret;
		}, `A new webhook secret was generated. Set it as the secret of the webhook in ${host}.`);
	}

	function remove(): void {
		if (!service) return;
		if (
			!confirm(
				`Delete the connection ${service.displayName}? Repositories that no other connection mirrors are removed.`
			)
		)
			return;
		void act(async () => {
			await graphql(DELETE_EXTERNAL_SERVICE, { id });
			await goto('/site-admin/external-services');
		});
	}

	async function copy(label: string, text: string): Promise<void> {
		try {
			await navigator.clipboard.writeText(text);
			copied = label;
			setTimeout(() => {
				if (copied === label) copied = null;
			}, 2000);
		} catch {
			copied = null;
		}
	}

	function showOlder(): void {
		jobsShown += JOBS_PAGE;
		void load(id, true);
	}

	const stateClass = (s: SyncJob['state']) =>
		s === 'COMPLETED'
			? 'badge-success'
			: s === 'FAILED' || s === 'ERRORED'
				? 'badge-danger'
				: isActive(s)
					? 'badge-info'
					: '';
</script>

<svelte:head><title>{service?.displayName ?? 'Code host connection'} · Site admin · Sourcegull</title></svelte:head>

<p class="crumbs"><a href="/site-admin/external-services">Code host connections</a> /</p>

{#if loading && !service}
	<p class="loading" aria-live="polite"><span class="spinner"></span> Loading…</p>
{:else if error && !service}
	<div class="alert alert-danger" role="alert">{error}</div>
{:else if service}
	<div class="page-header">
		<div>
			<h1>{service.displayName}</h1>
			<p class="page-description">{host} · {service.url}</p>
			<RelatedLinks />
		</div>
		<div class="page-actions">
			<button class="btn" type="button" onclick={testConnection} disabled={checking}>
				{checking ? 'Testing…' : 'Test connection'}
			</button>
			<button class="btn" type="button" onclick={syncNow} disabled={busy}>Sync now</button>
			{#if !service.managedBy}
				<a class="btn" href={editURL}>Edit</a>
				<button class="btn btn-danger" type="button" onclick={remove} disabled={busy}>Delete</button>
			{/if}
		</div>
	</div>

	<ManagedNotice managedBy={service.managedBy} />

	{#if notice}<div class="alert alert-info" role="status">{notice}</div>{/if}
	{#if actionError}<div class="alert alert-danger" role="alert">{actionError}</div>{/if}
	{#if error}<div class="alert alert-warning" role="alert">Could not refresh: {error}</div>{/if}

	{#if availability}
		<div class="check" aria-live="polite" data-testid="connection-check">
			{#if availability.__typename === 'ExternalServiceAvailable'}
				<div class="alert alert-success">
					<strong>Connection OK.</strong> {host} answered and accepted the token
					({formatDate(availability.lastCheckedAt)}).
				</div>
			{:else if availability.__typename === 'ExternalServiceUnavailable'}
				<div class="alert alert-danger">
					<strong>Connection problem.</strong>
					<pre class="error-text">{availability.suspectedReason}</pre>
				</div>
			{:else}
				<div class="alert alert-info">{availability.implementationNote}</div>
			{/if}
		</div>
	{/if}

	<section class="card section">
		<div class="card-header"><h2>Status</h2></div>
		<dl class="facts">
			<dt>Repositories</dt>
			<dd>
				<a href="/repositories">{service.repoCount.toLocaleString('en-US')}</a>
			</dd>
			<dt>Last sync</dt>
			<dd>
				{#if service.lastSyncAt}
					<span title={formatDate(service.lastSyncAt)}>{relativeTime(service.lastSyncAt)}</span>
				{:else}never{/if}
			</dd>
			<dt>Next sync</dt>
			<dd>
				{#if service.nextSyncAt}
					<span title={formatDate(service.nextSyncAt)}>{relativeTime(service.nextSyncAt)}</span>
				{:else}—{/if}
			</dd>
			<dt>Permissions</dt>
			<dd>
				{service.unrestricted
					? 'Unrestricted: every user can read the repositories (no authorization block).'
					: `Enforced from ${host} (authorization block).`}
			</dd>
			{#if service.rateLimiterState}
				{@const rl = service.rateLimiterState}
				<dt>API rate limit</dt>
				<dd data-testid="rate-limit">
					{rateLimitSummary(rl)}
					{#if rl.pausedUntil && Date.parse(rl.pausedUntil) > Date.now()}
						<div class="bad small">
							Paused: {host} asked to back off until {formatDate(rl.pausedUntil)}.
						</div>
					{/if}
				</dd>
			{/if}
			<dt>Created</dt>
			<dd>{formatDate(service.createdAt)} · updated {relativeTime(service.updatedAt)}</dd>
		</dl>
		{#if service.lastSyncError}
			<div class="alert alert-danger last-error">
				<strong>The last sync failed:</strong>
				<pre class="error-text">{service.lastSyncError}</pre>
			</div>
		{/if}
	</section>

	{#if nextSteps.some((s) => !s.done)}
		<div class="section">
			<SetupChecklist title="Next steps" titleId="next-steps" steps={nextSteps} />
		</div>
	{/if}

	<section class="card section webhook">
		<div class="card-header"><h2>Webhook</h2></div>
		<p class="muted">
			{#if service.kind === 'GITHUB'}
				Add a webhook in GitHub (repository or organization settings → Webhooks) with this URL, content
				type <code>application/json</code> and this secret, for the <em>Pushes</em> and
				<em>Repositories</em> events, so changes are fetched right away instead of at the next poll.
			{:else if service.kind === 'GITEA'}
				Add a Gitea webhook (repository or organization settings → Webhooks) with this URL, content type
				<code>application/json</code> and this secret, for the <em>Push</em>, <em>Create</em>,
				<em>Delete</em> and <em>Repository</em> events, so changes are fetched right away instead of at the
				next poll.
			{:else}
				Add a webhook in GitLab (project or group settings → Webhooks) with this URL and secret token,
				with push and merge request events, so changes are fetched right away instead of at the next
				poll.
			{/if}
		</p>
		<div class="field">
			<span class="label">URL</span>
			<div class="copy-row">
				<code class="value" data-testid="webhook-url">{service.webhookURL}</code>
				<button class="btn btn-sm" type="button" onclick={() => copy('url', service!.webhookURL)}>
					{copied === 'url' ? 'Copied' : 'Copy'}
				</button>
			</div>
		</div>
		<div class="field">
			<span class="label">Secret token</span>
			{#if newSecret}
				<div class="copy-row">
					<code class="value" data-testid="webhook-secret">{newSecret}</code>
					<button class="btn btn-sm" type="button" onclick={() => copy('secret', newSecret!)}>
						{copied === 'secret' ? 'Copied' : 'Copy'}
					</button>
				</div>
				<span class="muted" data-testid="webhook-secret-once"
					>Copy it now: it won't be shown again. Later only its last characters are shown.</span
				>
			{:else if service.webhookSecret}
				<div class="copy-row">
					<code class="value" data-testid="webhook-secret">{service.webhookSecret}</code>
				</div>
				<span class="muted">The secret isn't shown again after it is generated. Regenerate it to set a new one.</span>
			{:else}
				<span class="muted">No secret yet: webhooks are not accepted until one is configured.</span>
			{/if}
		</div>
		<div class="toolbar">
			{#if !service.managedBy}
				<button class="btn" type="button" onclick={regenerate} disabled={busy}>
					{service.webhookSecret ? 'Regenerate secret' : 'Generate secret'}
				</button>
			{/if}
			<a class="btn btn-ghost" href="/site-admin/webhook-logs?connection={encodeURIComponent(id)}">Webhook logs</a>
		</div>
	</section>

	<section class="section">
		<div class="section-header"><h2>Sync history</h2></div>
		<div class="table-scroll">
			<table class="table table-bordered table-sm jobs">
				<thead>
					<tr>
						<th>State</th>
						<th>Reason</th>
						<th>Queued</th>
						<th>Duration</th>
						<th class="num" title="Synced / added / modified / unmodified / deleted">Repositories</th>
						<th class="num">Errors</th>
						<th><span class="visually-hidden">Actions</span></th>
					</tr>
				</thead>
				<tbody>
					{#each jobs as job (job.id)}
						<tr data-job-state={job.state}>
							<td>
								<span class="state badge {stateClass(job.state)}">{SYNC_STATE_LABELS[job.state]}</span>
								{#if job.fullSync !== null}<div class="muted small">{job.fullSync ? 'full' : 'incremental'}</div>{/if}
							</td>
							<td>{reasonLabel(job.reason)}</td>
							<td><span title={formatDate(job.queuedAt)}>{relativeTime(job.queuedAt)}</span></td>
							<td>{duration(job.startedAt, job.finishedAt) ?? (job.startedAt ? 'running' : '—')}</td>
							<td class="counts num">
								{job.reposSynced} synced
								<div class="muted small">
									+{job.reposAdded} · ~{job.reposModified} · ={job.reposUnmodified} · −{job.reposDeleted}
								</div>
							</td>
							<td class="num">
								{#if job.repoSyncErrors > 0}<span class="bad">{job.repoSyncErrors}</span>{:else}0{/if}
							</td>
							<td class="actions">
								{#if isCancelable(job.state)}
									<button class="btn btn-sm" type="button" onclick={() => cancelJob(job)} disabled={busy}>Cancel</button>
								{/if}
							</td>
						</tr>
						{#if job.failureMessage}
							<tr class="failure">
								<td colspan="7"><pre class="error-text">{job.failureMessage}</pre></td>
							</tr>
						{/if}
						{#if job.warnings.length > 0}
							<tr class="failure">
								<td colspan="7"><pre class="warning-text">{job.warnings.join('\n')}</pre></td>
							</tr>
						{/if}
					{:else}
						<tr><td colspan="7" class="empty-cell">No syncs yet.</td></tr>
					{/each}
				</tbody>
			</table>
		</div>
		{#if jobs.length < jobsTotal}
			<div class="more"><button class="btn btn-sm" type="button" onclick={showOlder}>Show older syncs</button></div>
		{/if}
		<p class="muted small">
			Repository counts: + added, ~ modified, = unmodified, − removed. Incremental syncs list only
			projects active since the previous sync.
		</p>
	</section>
{/if}

<style>
	.crumbs {
		margin: 0 0 var(--space-2);
		font-size: var(--text-sm);
		color: var(--color-text-muted);
	}
	.page-header {
		align-items: flex-start;
	}
	.section {
		margin-bottom: var(--space-6);
	}
	.section > .section-header {
		margin-top: 0;
	}
	.facts {
		display: grid;
		grid-template-columns: max-content 1fr;
		gap: var(--space-2) var(--space-6);
		margin: 0;
	}
	.facts dt {
		font-weight: var(--weight-medium);
		color: var(--color-text-muted);
	}
	.facts dd {
		margin: 0;
		min-width: 0;
	}
	.last-error {
		margin: var(--space-4) 0 0;
	}
	.copy-row {
		display: flex;
		gap: var(--space-2);
		align-items: center;
		flex-wrap: wrap;
	}
	.value {
		font-family: var(--font-mono);
		font-size: var(--text-sm);
		background: var(--color-bg-subtle);
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
		padding: 0.2rem 0.5rem;
		overflow-wrap: anywhere;
		min-width: 0;
		max-width: 100%;
	}
	.warning-text {
		margin: var(--space-1) 0 0;
		white-space: pre-wrap;
		overflow-wrap: anywhere;
		font-size: var(--text-sm);
		color: var(--color-warning);
	}
		.error-text {
		margin: var(--space-1) 0 0;
		white-space: pre-wrap;
		overflow-wrap: anywhere;
		font-size: var(--text-sm);
	}
	.bad {
		color: var(--color-danger);
		font-weight: var(--weight-medium);
	}
	.failure td {
		padding-top: 0;
	}
	tr:has(+ tr.failure) > td {
		border-bottom: none;
	}
	.actions {
		text-align: right;
		white-space: nowrap;
	}
	.more {
		margin-top: var(--space-3);
	}
	@media (max-width: 640px) {
		.facts {
			grid-template-columns: 1fr;
			gap: 0 var(--space-4);
		}
		.facts dd {
			margin-bottom: var(--space-2);
		}
	}
</style>
