<!--
	One workspace's execution in detail: state,
	timing, queue position, failure message, the steps (run, container, `if:`, skipped or cached,
	output lines, output variables, diff stat, exit code), the setup and teardown stages, the
	changeset specs it produced, and retrying it. Polls while the execution runs.
-->
<script lang="ts">
	import { onDestroy } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		BATCH_SPEC_WORKSPACE,
		RETRY_WORKSPACE_EXECUTION,
		canRetryWorkspace,
		elapsed,
		formatDuration,
		formatOutputValue,
		isWorkspaceActive,
		parseOutputLine,
		queuePosition,
		stateLabel,
		stateTone,
		stepStatus,
		type BatchSpecWorkspaceResult,
		type LogEntry,
		type WorkspaceDetail
	} from './execution.ts';

	interface Props {
		id: string;
		/** The viewer may retry executions (the spec's creator or a site admin). */
		canModify: boolean;
		/** The workspace's state in the page's list: the details reload when it changes. */
		listState?: string;
		/** Called after a retry was queued (the page reloads the spec). */
		onchange?: () => void;
	}

	let { id, canModify, listState, onchange }: Props = $props();

	const POLL_MS = 2000;

	let ws = $state.raw<WorkspaceDetail | null>(null);
	let hidden = $state(false);
	let loading = $state(true);
	let error = $state<string | null>(null);
	let busy = $state(false);
	let actionError = $state<string | null>(null);
	let timer: ReturnType<typeof setTimeout> | undefined;
	let now = $state(Date.now());

	async function load(wsID: string): Promise<void> {
		clearTimeout(timer);
		try {
			const r = await graphql<BatchSpecWorkspaceResult, { id: string }>(BATCH_SPEC_WORKSPACE, { id: wsID });
			if (wsID !== id) return;
			error = null;
			hidden = r.node?.__typename === 'HiddenBatchSpecWorkspace';
			ws = r.node && r.node.__typename.endsWith('BatchSpecWorkspace') ? (r.node as WorkspaceDetail) : null;
			if (!ws) error = 'Workspace not found.';
			now = Date.now();
		} catch (err) {
			if (wsID === id) error = errorMessage(err);
		} finally {
			if (wsID === id) loading = false;
		}
		if (wsID === id && ws && isWorkspaceActive(ws.state)) timer = setTimeout(() => void load(wsID), POLL_MS);
	}

	let shown = '';
	$effect(() => {
		void listState;
		if (shown !== id) {
			shown = id;
			loading = true;
			ws = null;
		}
		void load(id);
	});
	onDestroy(() => clearTimeout(timer));

	async function retry(): Promise<void> {
		if (!ws) return;
		busy = true;
		actionError = null;
		try {
			await graphql<unknown, { workspaces: string[] }>(RETRY_WORKSPACE_EXECUTION, { workspaces: [ws.id] });
			await load(id);
			onchange?.();
		} catch (err) {
			actionError = errorMessage(err);
		} finally {
			busy = false;
		}
	}

	function entryDuration(e: LogEntry): string {
		return e.durationMilliseconds !== null ? formatDuration(e.durationMilliseconds) : e.exitCode === null ? 'running' : '';
	}

	const time = (iso: string | null) => (iso ? new Date(iso).toLocaleString() : '—');
</script>

<section class="detail" aria-label="Workspace details" aria-busy={loading}>
	{#if error}
		<div class="alert alert-danger" role="alert">{error}</div>
	{:else if !ws}
		<p class="muted">Loading…</p>
	{:else}
		<div class="head">
			<h2 class="title">
				{#if ws.repository}
					<a class="mono" href={ws.repository.url}>{ws.repository.name}</a>
				{:else}
					Hidden repository
				{/if}
			</h2>
			<span class="badge tone-{stateTone(ws.state)}" data-testid="workspace-state">{stateLabel(ws.state)}</span>
			<span class="spacer"></span>
			{#if canModify && canRetryWorkspace(ws)}
				<button class="btn btn-sm" type="button" disabled={busy} onclick={retry}>Retry workspace</button>
			{/if}
		</div>
		{#if hidden}
			<p class="muted">You may not read this repository; its execution is not shown.</p>
		{/if}
		<dl class="facts small">
			{#if ws.branch}<dt>Branch</dt><dd class="mono">{ws.branch.displayName}</dd>{/if}
			{#if ws.path !== undefined}<dt>Path</dt><dd class="mono">{ws.path || '/'}</dd>{/if}
			{#if queuePosition(ws)}<dt>Queue</dt><dd>{queuePosition(ws)}</dd>{/if}
			<dt>Queued</dt><dd>{time(ws.queuedAt)}</dd>
			<dt>Started</dt><dd>{time(ws.startedAt)}</dd>
			<dt>Finished</dt><dd>{time(ws.finishedAt)}</dd>
			{#if elapsed(ws.startedAt, ws.finishedAt, now) !== null}
				<dt>Duration</dt><dd>{formatDuration(elapsed(ws.startedAt, ws.finishedAt, now) ?? 0)}</dd>
			{/if}
			{#if ws.diffStat}
				<dt>Changes</dt>
				<dd><span class="added">+{ws.diffStat.added}</span> <span class="deleted">−{ws.diffStat.deleted}</span></dd>
			{/if}
			{#if ws.cachedResultFound}<dt>Cache</dt><dd>the whole result came from the cache</dd>
			{:else if ws.stepCacheResultCount > 0}<dt>Cache</dt><dd>{ws.stepCacheResultCount} cached steps</dd>{/if}
			{#if ws.executor}<dt>Executor</dt><dd class="mono">{ws.executor.hostname}</dd>{/if}
		</dl>
		{#if actionError}<div class="alert alert-danger" role="alert">{actionError}</div>{/if}
		{#if ws.failureMessage}
			<div class="alert alert-danger failure" role="alert" data-testid="workspace-failure">{ws.failureMessage}</div>
		{/if}

		{#if ws.changesetSpecs && ws.changesetSpecs.length > 0}
			<h3>Changeset specs</h3>
			<ul class="changesets">
				{#each ws.changesetSpecs as cs (cs.id)}
					<li>
						<strong>{cs.description.title ?? 'Imported changeset'}</strong>
						{#if cs.description.headRef}<span class="muted mono small"> {cs.description.headRef}</span>{/if}
						{#if cs.description.diffStat}
							<span class="small"
								><span class="added">+{cs.description.diffStat.added}</span>
								<span class="deleted">−{cs.description.diffStat.deleted}</span></span
							>
						{/if}
					</li>
				{/each}
			</ul>
		{/if}

		{#if ws.stages && ws.stages.setup.length > 0}
			<details class="stage">
				<summary>Setup ({ws.stages.setup.length})</summary>
				{@render entries(ws.stages.setup)}
			</details>
		{/if}

		{#if ws.steps && ws.steps.length > 0}
			<h3>Steps</h3>
			<ol class="steps" aria-label="Steps">
				{#each ws.steps as step (step.number)}
					{@const status = stepStatus(step)}
					<li class="step" data-step={step.number}>
						<details open={step.exitCode !== null && step.exitCode !== 0}>
							<summary>
								<span class="step-title">Step {step.number}</span>
								<span class="mono small container">{step.container}</span>
								<span class="badge tone-{status.tone}">{status.label}</span>
								{#if step.diffStat}
									<span class="small"
										><span class="added">+{step.diffStat.added}</span>
										<span class="deleted">−{step.diffStat.deleted}</span></span
									>
								{/if}
								{#if elapsed(step.startedAt, step.finishedAt, now) !== null}
									<span class="muted small">{formatDuration(elapsed(step.startedAt, step.finishedAt, now) ?? 0)}</span>
								{/if}
							</summary>
							<div class="step-body">
								{#if step.ifCondition}
									<div class="small"><span class="muted">if:</span> <code>{step.ifCondition}</code></div>
								{/if}
								<div class="small muted">run:</div>
								<pre class="code">{step.run}</pre>
								{#if step.environment.length > 0}
									<div class="small muted">Environment</div>
									<ul class="kv small">
										{#each step.environment as env (env.name)}
											<li>
												<code>{env.name}</code>
												{#if env.value === null}<span class="muted">(secret)</span>{:else}= <code>{env.value}</code>{/if}
											</li>
										{/each}
									</ul>
								{/if}
								{#if step.outputVariables && step.outputVariables.length > 0}
									<div class="small muted">Output variables</div>
									<ul class="kv small" aria-label="Output variables">
										{#each step.outputVariables as v (v.name)}
											<li><code>{v.name}</code> = <code class="value">{formatOutputValue(v.value)}</code></li>
										{/each}
									</ul>
								{/if}
								{#if step.outputLines.nodes.length > 0}
									<div class="small muted">Output</div>
									<pre class="log" aria-label="Step {step.number} output">{#each step.outputLines.nodes as raw, i (i)}{@const line = parseOutputLine(raw)}<span class:stderr={line.stream === 'stderr'}>{line.text}</span>{'\n'}{/each}</pre>
									{#if step.outputLines.totalCount > step.outputLines.nodes.length}
										<div class="muted small">
											Showing the first {step.outputLines.nodes.length} of {step.outputLines.totalCount} lines.
										</div>
									{/if}
								{:else if !step.skipped && !step.cachedResultFound && step.exitCode !== null}
									<div class="muted small">No output.</div>
								{/if}
								{#if step.exitCode !== null}<div class="small muted">Exit code {step.exitCode}</div>{/if}
							</div>
						</details>
					</li>
				{/each}
			</ol>
		{/if}

		{#if ws.stages && ws.stages.teardown.length > 0}
			<details class="stage">
				<summary>Teardown ({ws.stages.teardown.length})</summary>
				{@render entries(ws.stages.teardown)}
			</details>
		{/if}
	{/if}
</section>

{#snippet entries(list: LogEntry[])}
	<ul class="entries">
		{#each list as e, i (i)}
			<li>
				<div class="small">
					<code>{e.key}</code>
					{#if e.exitCode !== null}
						<span class="badge tone-{e.exitCode === 0 ? 'success' : 'danger'}">exit {e.exitCode}</span>
					{/if}
					<span class="muted">{entryDuration(e)}</span>
				</div>
				{#if e.command.length > 0}<code class="small cmd">{e.command.join(' ')}</code>{/if}
				{#if e.out}<pre class="log">{e.out}</pre>{/if}
			</li>
		{/each}
	</ul>
{/snippet}

<style>
	.detail {
		min-width: 0;
	}
	.head {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		flex-wrap: wrap;
	}
	.title {
		margin: 0;
		font-size: 1.05rem;
		overflow-wrap: anywhere;
	}
	.spacer {
		flex: 1;
	}
	h3 {
		font-size: 1rem;
		margin: var(--space-4) 0 var(--space-2);
	}
	.small {
		font-size: 0.875em;
	}
	.facts {
		display: grid;
		grid-template-columns: max-content minmax(0, 1fr);
		gap: var(--space-1) var(--space-3);
		margin: var(--space-3) 0;
	}
	.facts dt {
		color: var(--color-text-muted);
	}
	.facts dd {
		margin: 0;
		overflow-wrap: anywhere;
	}
	.failure {
		white-space: pre-wrap;
		overflow-wrap: anywhere;
	}
	.added {
		color: var(--color-success);
	}
	.deleted {
		color: var(--color-danger);
	}
	.changesets {
		margin: 0;
		padding-left: 1.25rem;
	}
	.steps {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
	}
	.step details,
	.stage {
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
	}
	.stage {
		margin-top: var(--space-3);
	}
	summary {
		cursor: pointer;
		padding: var(--space-2) var(--space-3);
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: var(--space-2);
	}
	.step-title {
		font-weight: 600;
	}
	.container {
		overflow-wrap: anywhere;
	}
	.step-body,
	.entries {
		padding: 0 var(--space-3) var(--space-3);
	}
	.entries {
		list-style: none;
		margin: 0;
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
	}
	.cmd {
		display: block;
		overflow-wrap: anywhere;
		white-space: pre-wrap;
	}
	.code,
	.log {
		margin: var(--space-1) 0 var(--space-2);
		padding: var(--space-2);
		max-height: 24rem;
		overflow: auto;
		background: var(--color-bg-subtle);
		border-radius: var(--radius);
		font-size: 0.8em;
		white-space: pre-wrap;
		overflow-wrap: anywhere;
	}
	.log .stderr {
		color: var(--color-danger);
	}
	.kv {
		list-style: none;
		margin: var(--space-1) 0 var(--space-2);
		padding: 0;
	}
	.kv .value {
		white-space: pre-wrap;
		overflow-wrap: anywhere;
	}
	.tone-success {
		background: var(--color-success-bg);
		color: var(--color-success);
	}
	.tone-danger {
		background: var(--color-danger-bg);
		color: var(--color-danger);
	}
	.tone-info {
		background: var(--color-info-bg);
		color: var(--color-primary);
	}
	.tone-warning {
		background: var(--color-warning-bg);
		color: var(--color-warning);
	}
</style>
