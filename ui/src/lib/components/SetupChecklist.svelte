<!--
	A checklist of setup steps: done ones ticked, open ones with a link to where they are done.
-->
<script lang="ts">
	import type { Snippet } from 'svelte';
	import Icon from '#lib/components/Icon.svelte';
	import type { SetupStep } from '#lib/admin/setup.ts';

	let {
		title,
		steps,
		titleId,
		actions
	}: { title: string; steps: SetupStep[]; titleId: string; actions?: Snippet } = $props();
	const done = $derived(steps.filter((s) => s.done).length);
</script>

<section class="card checklist" aria-labelledby={titleId}>
	<div class="head">
		<h2 id={titleId}>{title}</h2>
		<span class="small muted">{done} of {steps.length} done</span>
		{#if actions}<span class="actions">{@render actions()}</span>{/if}
	</div>
	<ol role="list">
		{#each steps as step (step.id)}
			<li class:done={step.done}>
				<span class="mark" aria-hidden="true">{#if step.done}<Icon name="check" size={14} />{/if}</span>
				<div class="body">
					<strong>{step.title}</strong>
					<span class="visually-hidden">{step.done ? '(done)' : '(to do)'}</span>
					{#if !step.done}<span class="small muted">{step.text}</span>{/if}
				</div>
				{#if !step.done && step.href}
					<a class="btn btn-sm btn-secondary" href={step.href}>{step.linkText}</a>
				{/if}
			</li>
		{/each}
	</ol>
</section>

<style>
	.checklist {
		padding: var(--space-4) var(--space-5);
	}
	.head {
		display: flex;
		align-items: baseline;
		flex-wrap: wrap;
		gap: var(--space-1) var(--space-3);
		margin-bottom: var(--space-3);
	}
	h2 {
		margin: 0;
		font-size: var(--text-md);
	}
	.actions {
		margin-left: auto;
	}
	ol {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-direction: column;
		gap: var(--space-3);
	}
	li {
		display: flex;
		align-items: flex-start;
		gap: var(--space-3);
	}
	.mark {
		display: inline-flex;
		align-items: center;
		justify-content: center;
		flex: none;
		width: 1.25rem;
		height: 1.25rem;
		margin-top: 0.1rem;
		border: 1.5px solid var(--color-border);
		border-radius: var(--radius-full);
	}
	.done .mark {
		border-color: var(--color-success-border);
		background: var(--color-success-bg);
		color: var(--color-success);
	}
	.done strong {
		font-weight: var(--weight-normal);
		color: var(--color-text-muted);
	}
	.body {
		flex: 1;
		min-width: 0;
		display: flex;
		flex-direction: column;
		gap: 0.125rem;
	}
	li .btn {
		flex: none;
	}
</style>
