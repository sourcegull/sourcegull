<script lang="ts">
	import { onMount } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		STATUS_MESSAGES,
		STATUS_MESSAGES_POLL_INTERVAL,
		stateLabel,
		summarize,
		type StatusMessagesResult,
		type StatusState,
		type StatusSummary
	} from '#lib/status-messages.ts';

	let summary = $state.raw<StatusSummary | null>(null);
	let error = $state<string | null>(null);
	let open = $state(false);
	let root: HTMLDivElement;

	async function poll(): Promise<void> {
		try {
			const result = await graphql<StatusMessagesResult>(STATUS_MESSAGES);
			summary = summarize(result.statusMessages);
			error = null;
		} catch (err) {
			error = errorMessage(err);
		}
	}

	onMount(() => {
		void poll();
		const timer = setInterval(() => {
			// A hidden tab does not need fresh status; it catches up when shown.
			if (document.visibilityState === 'visible') void poll();
		}, STATUS_MESSAGES_POLL_INTERVAL);
		const onVisible = () => {
			if (document.visibilityState === 'visible') void poll();
		};
		document.addEventListener('visibilitychange', onVisible);
		return () => {
			clearInterval(timer);
			document.removeEventListener('visibilitychange', onVisible);
		};
	});

	function onWindowClick(event: MouseEvent) {
		if (open && event.target instanceof Node && !root.contains(event.target)) open = false;
	}

	function onKeydown(event: KeyboardEvent) {
		if (open && event.key === 'Escape') {
			open = false;
			root.querySelector<HTMLButtonElement>('button.trigger')?.focus();
		}
	}

	const status = $derived<StatusState>(error ? 'error' : (summary?.state ?? 'ok'));
	const label = $derived(error ? 'Status unavailable' : stateLabel(status));
</script>

<svelte:window onclick={onWindowClick} onkeydown={onKeydown} />

<div class="status-nav" bind:this={root}>
	<button
		class="trigger btn btn-ghost btn-icon"
		type="button"
		aria-haspopup="dialog"
		aria-expanded={open}
		aria-label={label}
		title={label}
		data-state={status}
		onclick={() => {
			open = !open;
			if (open) void poll();
		}}
	>
		{#if status === 'error'}
			<svg viewBox="0 0 16 16" width="16" height="16" aria-hidden="true">
				<path d="M8 1.5 15 14H1z" fill="none" stroke="currentColor" stroke-width="1.5" />
				<path d="M8 6v3.5M8 11.2v.3" stroke="currentColor" stroke-width="1.5" />
			</svg>
		{:else if status === 'progress'}
			<svg class="spin" viewBox="0 0 16 16" width="16" height="16" aria-hidden="true">
				<path
					d="M14 8a6 6 0 1 1-1.76-4.24M14 2v3h-3"
					fill="none"
					stroke="currentColor"
					stroke-width="1.5"
				/>
			</svg>
		{:else}
			<svg viewBox="0 0 16 16" width="16" height="16" aria-hidden="true">
				<circle cx="8" cy="8" r="6.25" fill="none" stroke="currentColor" stroke-width="1.5" />
				<path d="m5 8.2 2 2 4-4.2" fill="none" stroke="currentColor" stroke-width="1.5" />
			</svg>
		{/if}
	</button>
	{#if open}
		<div class="popover" role="dialog" aria-label="Status messages">
			<div class="popover-header">Code sync status</div>
			{#if error}
				<div class="entry error">
					<strong>Could not load the status</strong>
					<p>{error}</p>
				</div>
			{:else if !summary || summary.entries.length === 0}
				<div class="entry ok">
					<strong>Repositories up to date</strong>
					<p>All repositories are cloned and indexed; no code host sync errors.</p>
				</div>
			{:else}
				{#each summary.entries as entry, i (i)}
					<div class="entry {entry.kind}">
						<strong>{entry.title}</strong>
						<p>{entry.text}</p>
						<a href={entry.href} onclick={() => (open = false)}>{entry.linkText}</a>
					</div>
				{/each}
			{/if}
		</div>
	{/if}
</div>

<style>
	.status-nav {
		position: relative;
	}
	.trigger {
		color: var(--color-success);
	}
	.trigger[data-state='progress'] {
		color: var(--color-primary);
	}
	.trigger[data-state='error'] {
		color: var(--color-danger);
	}
	.spin {
		animation: spin 1.6s linear infinite;
	}
	@media (prefers-reduced-motion: reduce) {
		.spin {
			animation: none;
		}
	}
	@keyframes spin {
		to {
			transform: rotate(360deg);
		}
	}
	.popover {
		position: absolute;
		right: 0;
		top: calc(100% + 6px);
		width: min(26rem, calc(100vw - 2rem));
		max-height: 70vh;
		overflow-y: auto;
		background: var(--color-surface-raised);
		border: 1px solid var(--color-border);
		border-radius: var(--radius-lg);
		box-shadow: var(--shadow-lg);
		z-index: 40;
	}
	.popover-header {
		padding: var(--space-2) var(--space-3);
		border-bottom: 1px solid var(--color-border);
		font-weight: var(--weight-semibold);
	}
	.entry {
		padding: var(--space-2) var(--space-3);
		border-left: 3px solid transparent;
	}
	.entry + .entry {
		border-top: 1px solid var(--color-border);
	}
	.entry p {
		margin: var(--space-1) 0;
		color: var(--color-text-muted);
		overflow-wrap: anywhere;
	}
	.entry.error {
		border-left-color: var(--color-danger);
	}
	.entry.warning {
		border-left-color: var(--color-warning);
	}
	.entry.progress {
		border-left-color: var(--color-primary);
	}
	.entry.ok {
		border-left-color: var(--color-success);
	}
</style>
