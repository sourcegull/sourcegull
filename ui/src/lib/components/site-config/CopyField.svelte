<script lang="ts">
	/** A read-only value (e.g. an OAuth redirect URI) with a copy button. */
	let { value, label }: { value: string; label: string } = $props();

	let copied = $state(false);
	let timer: ReturnType<typeof setTimeout> | undefined;

	async function copy(): Promise<void> {
		try {
			await navigator.clipboard.writeText(value);
			copied = true;
			clearTimeout(timer);
			timer = setTimeout(() => (copied = false), 2000);
		} catch {
			copied = false;
		}
	}
</script>

<span class="copy-field">
	<input class="input input-sm mono" readonly {value} aria-label={label} onfocus={(e) => e.currentTarget.select()} />
	<button class="btn btn-sm" type="button" onclick={copy} aria-label="Copy {label}">{copied ? 'Copied' : 'Copy'}</button>
	<span class="visually-hidden" aria-live="polite">{copied ? `${label} copied` : ''}</span>
</span>

<style>
	.copy-field {
		display: flex;
		gap: var(--space-2);
		margin: var(--space-1) 0;
		max-width: 40rem;
	}
	.copy-field .input {
		flex: 1 1 auto;
		min-width: 0;
	}
</style>
