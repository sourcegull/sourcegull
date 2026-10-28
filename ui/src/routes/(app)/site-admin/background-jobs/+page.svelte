<!--
	Background jobs: the worker's leader-elected routines (which process
	holds each, recent runs, errors and run statistics) and its job queues (counts by state,
	oldest queued job, recent failures with retry and cancel).
-->
<script lang="ts">
	import { onDestroy, onMount } from 'svelte';
	import { formatDate, relativeTime } from '#lib/batches/batch-change.ts';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		BACKGROUND_JOBS,
		CANCEL_QUEUE_JOB,
		RETRY_QUEUE_JOB,
		formatMs,
		isRunning,
		routineHealth,
		type BackgroundJob,
		type BackgroundJobsResult,
		type Queue,
		type QueueJob
	} from '#lib/admin/logs.ts';

	const REFRESH_MS = 10_000;
	const RECENT_RUNS = 5;

	let jobs = $state.raw<BackgroundJob[]>([]);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let notice = $state<string | null>(null);
	let busy = $state<string | null>(null);
	let onlyProblems = $state(false);
	let timer: ReturnType<typeof setInterval> | undefined;

	async function load(): Promise<void> {
		try {
			jobs = (
				await graphql<BackgroundJobsResult, { recentRunCount: number }>(BACKGROUND_JOBS, {
					recentRunCount: RECENT_RUNS
				})
			).backgroundJobs.nodes;
			error = null;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loading = false;
		}
	}

	async function act(q: Queue, job: QueueJob, kind: 'retry' | 'cancel'): Promise<void> {
		busy = `${q.table}/${job.id}`;
		notice = null;
		try {
			await graphql(kind === 'retry' ? RETRY_QUEUE_JOB : CANCEL_QUEUE_JOB, {
				queue: q.table,
				id: job.id
			});
			notice =
				kind === 'retry'
					? `Job ${job.id} of ${q.table} was queued again.`
					: `Cancellation of job ${job.id} of ${q.table} was requested.`;
			await load();
		} catch (err) {
			error = errorMessage(err);
		} finally {
			busy = null;
		}
	}

	const visible = (job: BackgroundJob) =>
		onlyProblems ? job.routines.filter((r) => routineHealth(r) === 'failing') : job.routines;

	const HEALTH_LABELS = { ok: 'OK', failing: 'Failing', stopped: 'Stopped', idle: 'Not run yet' };

	onMount(() => {
		void load();
		timer = setInterval(() => void load(), REFRESH_MS);
	});
	onDestroy(() => clearInterval(timer));
</script>

<svelte:head><title>Background jobs · Site admin · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Background jobs</h1>
		<p class="page-description">
			Routines that one worker runs at a time under a lease, and the job queues every worker takes
			jobs from. Runs and lease holders are kept for seven days; the page refreshes every ten seconds.
		</p>
	</div>
	<div class="page-actions">
		<label class="checkbox">
			<input type="checkbox" bind:checked={onlyProblems} /> Only failing
		</label>
	</div>
</div>

{#if notice}<div class="alert alert-info" role="status">{notice}</div>{/if}
{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}
{#if loading && jobs.length === 0}<p class="loading" aria-live="polite"><span class="spinner"></span> Loading…</p>{/if}

{#each jobs as job (job.id)}
	<section class="section">
		<div class="section-header"><h2>{job.name}</h2></div>
		{#if visible(job).length === 0}
			<p class="muted">{onlyProblems ? 'Nothing is failing.' : 'None.'}</p>
		{/if}
		{#each visible(job) as r (r.name)}
			{@const health = routineHealth(r)}
			<article class="card routine" data-routine={r.name} data-health={health}>
				<header>
					<h3 class="mono">{r.name}</h3>
					<span
						class="badge health {health}"
						class:badge-success={health === 'ok'}
						class:badge-danger={health === 'failing'}
						class:badge-warning={health === 'stopped'}>{HEALTH_LABELS[health]}</span
					>
				</header>
				{#if r.description}<p class="muted">{r.description}</p>{/if}

				{#if r.queue}
					{@const q = r.queue}
					<dl class="counts">
						<div><dt>Queued</dt><dd>{q.queued.toLocaleString('en-US')}</dd></div>
						<div><dt>Processing</dt><dd>{q.processing.toLocaleString('en-US')}</dd></div>
						<div><dt>Completed</dt><dd>{q.completed.toLocaleString('en-US')}</dd></div>
						<div><dt>Errored</dt><dd class:bad={q.errored > 0}>{q.errored.toLocaleString('en-US')}</dd></div>
						<div><dt>Failed</dt><dd class:bad={q.failed > 0}>{q.failed.toLocaleString('en-US')}</dd></div>
						<div><dt>Canceled</dt><dd>{q.canceled.toLocaleString('en-US')}</dd></div>
						<div>
							<dt>Oldest queued</dt>
							<dd>
								{#if q.oldestQueuedAt}
									<span title={formatDate(q.oldestQueuedAt)}>{relativeTime(q.oldestQueuedAt)}</span>
								{:else}—{/if}
							</dd>
						</div>
					</dl>
					{#if q.activeJobs.length > 0}
						<details>
							<summary>Oldest queued and processing jobs</summary>
							<div class="table-scroll">
								<table class="table table-bordered table-sm">
									<thead>
										<tr>
											<th>Job</th>
											<th>Queued</th>
											<th>Started</th>
											<th>Attempts</th>
											<th><span class="visually-hidden">Actions</span></th>
										</tr>
									</thead>
									<tbody>
										{#each q.activeJobs as j (j.id)}
											<tr>
												<td class="mono">{j.id}<div class="muted small">{j.state}</div></td>
												<td class="small" title={formatDate(j.queuedAt)}>{relativeTime(j.queuedAt)}</td>
												<td class="small">
													{j.startedAt ? relativeTime(j.startedAt) : '—'}
													{#if j.workerHostname}<div class="muted">{j.workerHostname}</div>{/if}
												</td>
												<td>{j.numFailures} failures, {j.numResets} resets</td>
												<td class="actions">
													<button
														class="btn btn-sm"
														type="button"
														disabled={busy === `${q.table}/${j.id}`}
														title={j.state === 'processing'
															? 'Asks the worker to stop the job'
															: 'Cancels the job before it runs'}
														onclick={() => void act(q, j, 'cancel')}>Cancel</button
													>
												</td>
											</tr>
										{/each}
									</tbody>
								</table>
							</div>
						</details>
					{/if}
					{#if q.recentFailures.length > 0}
						<details>
							<summary>Recent failures</summary>
							<div class="table-scroll">
								<table class="table table-bordered table-sm">
									<thead>
										<tr>
											<th>Job</th>
											<th>Finished</th>
											<th>Attempts</th>
											<th>Error</th>
											<th><span class="visually-hidden">Actions</span></th>
										</tr>
									</thead>
									<tbody>
										{#each q.recentFailures as f (f.id)}
											<tr>
												<td class="mono">{f.id}<div class="muted small">{f.state}</div></td>
												<td class="small">
													{f.finishedAt ? relativeTime(f.finishedAt) : '—'}
													{#if f.workerHostname}<div class="muted">{f.workerHostname}</div>{/if}
												</td>
												<td>{f.numFailures} failures, {f.numResets} resets</td>
												<td><pre class="error-text">{f.failureMessage ?? ''}</pre></td>
												<td class="actions">
													<button
														class="btn btn-sm"
														type="button"
														disabled={busy === `${q.table}/${f.id}`}
														onclick={() => void act(q, f, 'retry')}>Retry</button
													>
												</td>
											</tr>
										{/each}
									</tbody>
								</table>
							</div>
						</details>
					{/if}
				{:else}
					<dl class="facts">
						<dt>Interval</dt>
						<dd>{r.intervalMs ? `every ${formatMs(r.intervalMs)}` : 'set by the site configuration or events'}</dd>
						<dt>Instances</dt>
						<dd>
							{#each r.instances as i (i.hostName)}
								<div>
									<span class="mono">{i.hostName}</span>
									{#if isRunning(i)}
										<span class="ok">holds the lease</span> since {relativeTime(i.lastStartedAt!)}
									{:else}
										<span class="muted"
											>stopped {i.lastStoppedAt ? relativeTime(i.lastStoppedAt) : ''}</span
										>
									{/if}
								</div>
							{:else}
								<span class="muted">No process has run it in the last seven days.</span>
							{/each}
						</dd>
						<dt>Last 7 days</dt>
						<dd>
							{r.stats.runCount.toLocaleString('en-US')} runs,
							<span class:bad={r.stats.errorCount > 0}
								>{r.stats.errorCount.toLocaleString('en-US')} errors</span
							>{#if r.stats.runCount > 0}; duration min {formatMs(r.stats.minDurationMs)}, avg {formatMs(
									r.stats.avgDurationMs
								)}, max {formatMs(r.stats.maxDurationMs)}{/if}
						</dd>
					</dl>
					{#if r.recentRuns.length > 0}
						<details>
							<summary>Recent runs</summary>
							<ul class="runs">
								{#each r.recentRuns as run, idx (idx)}
									<li>
										<span title={formatDate(run.at)}>{relativeTime(run.at)}</span>
										· {formatMs(run.durationMs)} · <span class="mono">{run.hostName}</span>
										{#if run.errorMessage}<pre class="error-text">{run.errorMessage}</pre>{/if}
									</li>
								{/each}
							</ul>
						</details>
					{/if}
				{/if}
			</article>
		{/each}
	</section>
{/each}

<style>
	.section {
		margin-bottom: var(--space-4);
	}
	.section:first-of-type .section-header {
		margin-top: 0;
	}
	.routine {
		margin-bottom: var(--space-3);
	}
	.routine header {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		flex-wrap: wrap;
	}
	.routine h3 {
		margin: 0;
		font-size: var(--text-md);
	}
	.routine > p {
		margin: var(--space-1) 0 0;
	}
	.bad {
		color: var(--color-danger);
	}
	.ok {
		color: var(--color-success);
	}
	details {
		margin-top: var(--space-3);
	}
	summary {
		cursor: pointer;
		font-weight: var(--weight-medium);
		color: var(--color-text-muted);
	}
	details .table-scroll {
		margin-top: var(--space-2);
	}
	.counts {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(7rem, 1fr));
		gap: var(--space-2);
		margin: var(--space-3) 0 0;
	}
	.counts > div {
		padding: var(--space-2) var(--space-3);
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
		background: var(--color-bg-subtle);
	}
	.counts dt {
		font-size: var(--text-xs);
		color: var(--color-text-muted);
	}
	.counts dd {
		margin: 0;
		font-size: var(--text-lg);
		font-weight: var(--weight-semibold);
		font-variant-numeric: tabular-nums;
	}
	.facts {
		display: grid;
		grid-template-columns: max-content minmax(0, 1fr);
		gap: var(--space-1) var(--space-3);
		margin: var(--space-3) 0 0;
	}
	.facts dt {
		color: var(--color-text-muted);
	}
	.facts dd {
		margin: 0;
	}
	.runs {
		list-style: none;
		padding: 0;
	}
	.small {
		font-size: var(--text-xs);
	}
	.actions {
		white-space: nowrap;
	}
	pre.error-text {
		margin: 0;
		white-space: pre-wrap;
		overflow-wrap: anywhere;
		font-size: var(--text-xs);
	}
</style>
