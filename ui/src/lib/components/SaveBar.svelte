<!--
	The bar that stays at the bottom of the view while a form has unsaved changes. Place it after
	the form, as a direct child of the element that scrolls with it, so it sticks for the whole form.
-->
<script lang="ts">
	import type { Snippet } from 'svelte';

	interface Props {
		dirty: boolean;
		saving: boolean;
		/** Whether saving is not possible right now, for example after a conflict. */
		saveDisabled?: boolean;
		/** What saving does, next to the "Unsaved changes" badge. */
		note: string;
		/** When the last save finished, for a short confirmation in the bar. */
		savedAt?: number | null;
		ondiscard: () => void;
		onsave: () => void;
		/** Problems with the save, shown in the bar so they are in view wherever the form is scrolled. */
		children?: Snippet;
	}

	let { dirty, saving, saveDisabled = false, note, savedAt = null, ondiscard, onsave, children }: Props = $props();

	const SAVED_SHOWN_MS = 4000;
	let savedShown = $state(false);
	$effect(() => {
		if (savedAt === null) return;
		savedShown = true;
		const timer = setTimeout(() => (savedShown = false), SAVED_SHOWN_MS);
		return () => clearTimeout(timer);
	});
</script>

{#if dirty || saving}
	<div class="save-bar" role="region" aria-label="Unsaved changes">
		{#if children}<div class="problems">{@render children()}</div>{/if}
		<span class="save-status" aria-live="polite">
			<span class="badge badge-warning">Unsaved changes</span>
			<span class="muted small">{note}</span>
		</span>
		<span class="save-actions">
			<button class="btn" type="button" onclick={ondiscard} disabled={!dirty || saving}>Discard</button>
			<button class="btn btn-primary" type="button" onclick={onsave} disabled={!dirty || saving || saveDisabled}>
				{saving ? 'Saving…' : 'Save'}
			</button>
		</span>
	</div>
{:else if savedShown}
	<div class="save-bar" role="status" data-testid="save-bar-saved">
		<span class="save-status"><span class="badge badge-success">Saved</span></span>
	</div>
{/if}

<style>
	.save-bar {
		position: sticky;
		bottom: 0;
		z-index: 5;
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		justify-content: space-between;
		gap: var(--space-2) var(--space-4);
		margin-top: var(--space-4);
		padding: var(--space-3) var(--space-4);
		background: var(--color-surface-raised);
		border: 1px solid var(--color-border-strong);
		border-radius: var(--radius-lg);
		box-shadow: var(--shadow-lg);
	}
	.problems {
		flex: 1 0 100%;
		min-width: 0;
	}
	.problems :global(.alert) {
		margin: 0;
	}
	.save-status,
	.save-actions {
		display: inline-flex;
		flex-wrap: wrap;
		align-items: center;
		gap: var(--space-2);
	}
</style>
