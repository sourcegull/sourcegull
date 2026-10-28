<!--
	The live preview card of the creation pages: once the fields it needs are valid, the
	trend is counted on the fly by the server (nothing is stored) and drawn as its card would
	be. It reloads when those fields change (debounced) and on "Refresh".
-->
<script lang="ts">
	import { errorMessage } from '#lib/graphql.ts';
	import PreviewChart from './PreviewChart.svelte';
	import { previewModelEmpty, type PreviewModel } from './creation.ts';

	interface Props {
		/** Whether the fields the preview needs are valid. */
		ready: boolean;
		/** The preview's inputs as a key: a change reloads it. */
		request: string;
		load: (signal: AbortSignal) => Promise<PreviewModel>;
		/** Shown while the preview is not ready. */
		blankMessage: string;
		/** A note under the chart (e.g. that it is a sample of the real chart). */
		note?: string;
	}

	let { ready, request, load, blankMessage, note }: Props = $props();

	type State =
		| { status: 'intact' }
		| { status: 'loading' }
		| { status: 'error'; error: string }
		| { status: 'data'; model: PreviewModel };

	let preview = $state.raw<State>({ status: 'intact' });
	let refreshes = $state(0);

	$effect(() => {
		void refreshes;
		if (!ready) {
			preview = { status: 'intact' };
			return;
		}
		void request;
		const controller = new AbortController();
		preview = { status: 'loading' };
		const timer = setTimeout(async () => {
			try {
				const model = await load(controller.signal);
				if (!controller.signal.aborted) preview = { status: 'data', model };
			} catch (err) {
				if (!controller.signal.aborted) preview = { status: 'error', error: errorMessage(err) };
			}
		}, 500);
		return () => {
			clearTimeout(timer);
			controller.abort();
		};
	});
</script>

<section class="card live-preview" aria-label="Live preview">
	<header>
		<h2>Live preview</h2>
		<button
			type="button"
			class="btn btn-sm"
			disabled={!ready || preview.status === 'loading'}
			onclick={() => refreshes++}>Refresh</button
		>
	</header>
	<div class="body" data-testid="live-preview" data-status={preview.status}>
		{#if preview.status === 'intact'}
			<p class="muted blank">{blankMessage}</p>
		{:else if preview.status === 'loading'}
			<p class="muted blank" role="status">Loading trend</p>
		{:else if preview.status === 'error'}
			<div class="alert alert-danger" role="alert">{preview.error}</div>
		{:else if previewModelEmpty(preview.model)}
			<div class="blank" role="status">
				<strong>No data to display</strong>
				<small class="muted">We couldn’t find any matches for this trend.</small>
			</div>
		{:else}
			<PreviewChart model={preview.model} />
		{/if}
	</div>
	{#if note}<p class="muted note">{note}</p>{/if}
</section>

<style>
	.live-preview {
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
	}
	header {
		display: flex;
		align-items: center;
		justify-content: space-between;
		gap: var(--space-2);
	}
	h2 {
		margin: 0;
		font-size: 1rem;
	}
	.body {
		height: 20rem;
		display: flex;
		flex-direction: column;
	}
	.body > :global(.preview-chart) {
		flex: 1;
	}
	.blank {
		flex: 1;
		display: flex;
		flex-direction: column;
		align-items: center;
		justify-content: center;
		gap: var(--space-1);
		margin: 0;
		padding: var(--space-3);
		text-align: center;
		border: 1px dashed var(--color-border);
		border-radius: 6px;
	}
	.note {
		margin: 0;
		font-size: 0.85rem;
	}
</style>
