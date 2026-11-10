<script lang="ts">
	import CountTile from '#lib/components/CountTile.svelte';
	import Pager from '#lib/components/Pager.svelte';
	import RelatedLinks from '#lib/components/RelatedLinks.svelte';
	import { onMount } from 'svelte';
	import { cursorPages, firstPage, hasPreviousPage, pageAfter, pageLoaded, previousPage } from '#lib/cursor-pages.ts';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		CANCEL_PERMISSIONS_SYNC_JOB,
		PERMISSIONS_SUBJECTS,
		PERMISSIONS_SYNC_JOBS,
		REPOSITORY_ACCESS_EXPLANATION,
		SCHEDULE_REPOSITORY_PERMISSIONS_SYNC,
		SCHEDULE_USER_PERMISSIONS_SYNC,
		type PermissionsSubjectsResult,
		type PermissionsSubjectsVariables,
		type PermissionsSyncJobNode,
		type PermissionsSyncJobState,
		type PermissionsSyncJobsResult,
		type PermissionsSyncJobsVariables,
		type RepositoryAccessExplanation,
		type RepositoryAccessExplanationResult,
		type RepositoryAccessExplanationVariables
	} from '#lib/queries.ts';

	const STATES: PermissionsSyncJobState[] = ['QUEUED', 'PROCESSING', 'ERRORED', 'FAILED', 'COMPLETED', 'CANCELED'];

	// "Why can X see Y".
	let username = $state('');
	let repoName = $state('');
	let subjects = $state<PermissionsSubjectsResult | null>(null);
	let explanation = $state<RepositoryAccessExplanation | null>(null);
	let explaining = $state(false);
	let explainError = $state<string | null>(null);
	let notice = $state<string | null>(null);

	async function explain(event: SubmitEvent): Promise<void> {
		event.preventDefault();
		explaining = true;
		explainError = null;
		notice = null;
		explanation = null;
		subjects = null;
		try {
			const found = await graphql<PermissionsSubjectsResult, PermissionsSubjectsVariables>(PERMISSIONS_SUBJECTS, {
				username: username.trim(),
				repo: repoName.trim()
			});
			subjects = found;
			if (!found.user) {
				explainError = `No user named ${JSON.stringify(username.trim())}.`;
				return;
			}
			if (!found.repository) {
				explainError = `No repository named ${JSON.stringify(repoName.trim())}.`;
				return;
			}
			const result = await graphql<RepositoryAccessExplanationResult, RepositoryAccessExplanationVariables>(
				REPOSITORY_ACCESS_EXPLANATION,
				{
					user: found.user.id,
					repository: found.repository.id
				}
			);
			explanation = result.repositoryAccessExplanation;
		} catch (err) {
			explainError = errorMessage(err);
		} finally {
			explaining = false;
		}
	}

	async function syncNow(kind: 'user' | 'repository'): Promise<void> {
		explainError = null;
		notice = null;
		try {
			if (kind === 'user' && subjects?.user) {
				await graphql(SCHEDULE_USER_PERMISSIONS_SYNC, { user: subjects.user.id });
				notice = `Permissions sync of ${subjects.user.username} queued.`;
			} else if (kind === 'repository' && subjects?.repository) {
				await graphql(SCHEDULE_REPOSITORY_PERMISSIONS_SYNC, {
					repository: subjects.repository.id
				});
				notice = `Permissions sync of ${subjects.repository.name} queued.`;
			}
			// The new job is the newest: show the first page.
			firstPage(pages);
			await loadJobs();
		} catch (err) {
			explainError = errorMessage(err);
		}
	}

	// Sync jobs.
	const pages = $state(cursorPages(50));
	let jobs = $state<PermissionsSyncJobNode[]>([]);
	let totalCount = $state(0);
	let counts = $state<PermissionsSyncJobsResult['permissionsSyncJobCounts'] | null>(null);
	let stateFilter = $state<PermissionsSyncJobState | ''>('');
	let loading = $state(true);
	let jobsError = $state<string | null>(null);
	let pending = $state<string | null>(null);
	let requestSeq = 0;

	async function loadJobs(): Promise<void> {
		const seq = ++requestSeq;
		loading = true;
		jobsError = null;
		try {
			const result = await graphql<PermissionsSyncJobsResult, PermissionsSyncJobsVariables>(PERMISSIONS_SYNC_JOBS, {
				first: pages.size,
				after: pageAfter(pages),
				state: stateFilter || null
			});
			if (seq !== requestSeq) return;
			jobs = result.permissionsSyncJobs.nodes;
			totalCount = result.permissionsSyncJobs.totalCount;
			counts = result.permissionsSyncJobCounts;
			pageLoaded(pages, result.permissionsSyncJobs.pageInfo);
			// The page emptied (jobs pruned, or a job left the filtered state): step back.
			if (jobs.length === 0 && hasPreviousPage(pages)) {
				previousPage(pages);
				void loadJobs();
				return;
			}
		} catch (err) {
			if (seq === requestSeq) jobsError = errorMessage(err);
		} finally {
			if (seq === requestSeq) loading = false;
		}
	}

	function restartJobs(): void {
		firstPage(pages);
		void loadJobs();
	}

	/** Shows only jobs in `state`; a second click shows every job again. */
	function showOnly(state: PermissionsSyncJobState | ''): void {
		stateFilter = stateFilter === state ? '' : state;
		restartJobs();
	}

	async function cancel(job: PermissionsSyncJobNode): Promise<void> {
		pending = job.id;
		jobsError = null;
		try {
			await graphql(CANCEL_PERMISSIONS_SYNC_JOB, { job: job.id });
			await loadJobs();
		} catch (err) {
			jobsError = errorMessage(err);
		} finally {
			pending = null;
		}
	}

	const formatTime = (iso: string | null) => (iso ? new Date(iso).toLocaleString() : 'never');
	const subjectName = (job: PermissionsSyncJobNode) =>
		job.subject === null ? '(deleted)' : job.subject.__typename === 'User' ? job.subject.username : job.subject.name;
	const providerLabel = (type: string) => ({ gitlab: 'GitLab', github: 'GitHub', gitea: 'Gitea' })[type] ?? type;
	const reasonLabel = (kind: string) => kind.toLowerCase().replaceAll('_', ' ');

	onMount(() => {
		void loadJobs();
	});
</script>

<svelte:head><title>Permissions · Site admin · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Repository permissions</h1>
		<p class="page-description">
			Repository access is synced from the code hosts of connections with an
			<code class="code-inline">authorization</code> block. Explain why a user can or cannot read a repository, and watch
			the permission sync jobs.
		</p>
		<RelatedLinks />
	</div>
</div>

<section class="card explain" aria-labelledby="explain-heading">
	<div class="card-header"><h2 id="explain-heading">Why can a user see a repository?</h2></div>
	<form class="explain-form" onsubmit={explain}>
		<div class="field">
			<label class="label" for="explain-user">Username</label>
			<input id="explain-user" class="input" required bind:value={username} autocomplete="off" />
		</div>
		<div class="field">
			<label class="label" for="explain-repo">Repository</label>
			<input
				id="explain-repo"
				class="input"
				required
				placeholder="gitlab.example.com/group/project"
				bind:value={repoName}
				autocomplete="off"
			/>
		</div>
		<button class="btn btn-primary" type="submit" disabled={explaining}>
			{explaining ? 'Checking…' : 'Explain'}
		</button>
	</form>

	{#if explainError}
		<div class="alert alert-danger" role="alert">{explainError}</div>
	{/if}
	{#if notice}
		<div class="alert alert-success" role="status">{notice}</div>
	{/if}

	{#if explanation && subjects?.user && subjects.repository}
		<div class="alert {explanation.canRead ? 'alert-success' : 'alert-warning'}" role="status">
			<strong>
				{subjects.user.username}
				{explanation.canRead ? 'can' : 'cannot'} read {subjects.repository.name}.
			</strong>
			{#if explanation.reasons.length}
				<ul>
					{#each explanation.reasons as reason, i (i)}
						<li>
							<span class="badge">{reasonLabel(reason.kind)}</span>
							{reason.detail}
							{#if reason.grantedAt}<span class="muted">({formatTime(reason.grantedAt)})</span>{/if}
						</li>
					{/each}
				</ul>
			{:else}
				<p>{explanation.summary}</p>
			{/if}
		</div>
		<dl class="synced">
			<dt>User permissions last synced</dt>
			<dd>
				{formatTime(explanation.userSyncedAt)}
				<button class="btn btn-sm" type="button" onclick={() => syncNow('user')}>Sync now</button>
			</dd>
			<dt>Repository permissions last synced</dt>
			<dd>
				{formatTime(explanation.repositorySyncedAt)}
				<button class="btn btn-sm" type="button" onclick={() => syncNow('repository')}>Sync now</button>
			</dd>
		</dl>
	{/if}
</section>

<section aria-labelledby="jobs-heading">
	<div class="section-header">
		<h2 id="jobs-heading">Sync jobs</h2>
	</div>

	<div class="tiles" role="group" aria-label="Show sync jobs">
		<CountTile
			value={counts?.total ?? null}
			label="jobs"
			active={stateFilter === ''}
			title="Show jobs in every state"
			onclick={() => showOnly('')}
		/>
		<CountTile
			value={counts?.queued ?? null}
			label="queued"
			active={stateFilter === 'QUEUED'}
			title="Show only queued jobs"
			disabled={!counts?.queued && stateFilter !== 'QUEUED'}
			onclick={() => showOnly('QUEUED')}
		/>
		<CountTile
			value={counts?.processing ?? null}
			label="processing"
			tone={counts?.processing ? 'primary' : undefined}
			active={stateFilter === 'PROCESSING'}
			title="Show only jobs that are running"
			disabled={!counts?.processing && stateFilter !== 'PROCESSING'}
			onclick={() => showOnly('PROCESSING')}
		/>
		<CountTile
			value={counts?.errored ?? null}
			label="errored"
			tone={counts?.errored ? 'warning' : undefined}
			active={stateFilter === 'ERRORED'}
			title="Show only jobs that failed and will be retried"
			disabled={!counts?.errored && stateFilter !== 'ERRORED'}
			onclick={() => showOnly('ERRORED')}
		/>
		<CountTile
			value={counts?.failed ?? null}
			label="failed"
			tone={counts?.failed ? 'warning' : undefined}
			active={stateFilter === 'FAILED'}
			title="Show only jobs that failed for good"
			disabled={!counts?.failed && stateFilter !== 'FAILED'}
			onclick={() => showOnly('FAILED')}
		/>
	</div>

	{#if jobsError}
		<div class="alert alert-danger" role="alert">{jobsError}</div>
	{/if}

	<div class="list-bar">
		<label class="visually-hidden" for="job-state">State</label>
		<select id="job-state" class="input input-sm state" bind:value={stateFilter} onchange={restartJobs}>
			<option value="">All states</option>
			{#each STATES as s (s)}<option value={s}>{s.toLowerCase()}</option>{/each}
		</select>
		<button class="btn btn-sm" type="button" onclick={loadJobs}>Refresh</button>
		{#if stateFilter}
			<span class="shown">
				<strong>Only {stateFilter.toLowerCase()} jobs</strong>
				<button type="button" class="btn btn-link" onclick={() => showOnly('')}>Show all jobs</button>
			</span>
		{/if}
		<Pager {pages} shown={jobs.length} total={totalCount} {loading} onchange={loadJobs} />
	</div>

	<div class="table-scroll">
		<table class="table table-bordered table-sm" aria-busy={loading}>
			<thead>
				<tr>
					<th scope="col">Subject</th>
					<th scope="col">State</th>
					<th scope="col">Reason</th>
					<th scope="col">Queued</th>
					<th scope="col" class="num">Took</th>
					<th scope="col" class="num">Added / removed / found</th>
					<th scope="col"><span class="visually-hidden">Actions</span></th>
				</tr>
			</thead>
			<tbody>
				{#each jobs as job (job.id)}
					<tr>
						<td>
							<span class="muted">{job.subject?.__typename === 'Repository' ? 'repo' : 'user'}</span>
							<strong>{subjectName(job)}</strong>
						</td>
						<td>
							<span
								class="badge"
								class:badge-success={job.state === 'COMPLETED'}
								class:badge-info={job.state === 'PROCESSING'}
								class:badge-danger={job.state === 'FAILED' || job.state === 'ERRORED'}>{job.state.toLowerCase()}</span
							>
							{#each job.codeHostStates.filter((s) => s.status === 'ERROR') as s (s.providerID)}
								<div class="muted failure">{providerLabel(s.providerType)}: {s.message}</div>
							{:else}
								{#if job.failureMessage}<div class="muted failure">{job.failureMessage}</div>{/if}
							{/each}
						</td>
						<td>{job.reason.replaceAll('_', ' ')}</td>
						<td>{formatTime(job.queuedAt)}</td>
						<td class="num">{job.ranForMs === null ? '' : `${job.ranForMs} ms`}</td>
						<td class="num">{job.permissionsAdded} / {job.permissionsRemoved} / {job.permissionsFound}</td>
						<td class="actions">
							{#if job.state === 'QUEUED' || job.state === 'PROCESSING'}
								<button class="btn btn-sm" type="button" disabled={pending === job.id} onclick={() => cancel(job)}
									>Cancel</button
								>
							{/if}
						</td>
					</tr>
				{:else}
					<tr>
						<td colspan="7" class="empty-cell">
							{#if loading}Loading…{:else if stateFilter}No {stateFilter.toLowerCase()} sync jobs.{:else}No sync jobs.{/if}
						</td>
					</tr>
				{/each}
			</tbody>
		</table>
	</div>
</section>

<style>
	.explain {
		margin-bottom: var(--space-6);
	}
	.explain-form {
		display: flex;
		flex-wrap: wrap;
		gap: var(--space-3);
		align-items: flex-end;
		margin-bottom: var(--space-4);
	}
	.explain-form .field {
		flex: 1 1 14rem;
		margin-bottom: 0;
	}
	.synced {
		display: grid;
		grid-template-columns: max-content 1fr;
		gap: var(--space-2) var(--space-4);
		margin: 0;
	}
	.synced dt {
		color: var(--color-text-muted);
	}
	.synced dd {
		margin: 0;
		display: flex;
		gap: var(--space-3);
		align-items: center;
	}
	.tiles {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(8rem, 10rem));
		gap: var(--space-2);
		margin-bottom: var(--space-4);
	}
	.list-bar {
		display: flex;
		align-items: center;
		flex-wrap: wrap;
		gap: var(--space-2) var(--space-4);
		margin-bottom: var(--space-3);
	}
	.list-bar .state {
		width: auto;
	}
	.shown {
		display: inline-flex;
		align-items: center;
		gap: var(--space-2);
	}
	.failure {
		font-size: var(--text-xs);
		max-width: 24rem;
	}
	.actions {
		text-align: right;
	}
</style>
