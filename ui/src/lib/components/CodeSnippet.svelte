<script lang="ts">
	/** A block of code or text to paste somewhere, with a copy button. */
	interface Props {
		code: string;
		/** Accessible name of the copy button ("Copy <label>"). */
		label: string;
		/** Wrap long lines instead of scrolling (prose such as prompts). */
		wrap?: boolean;
	}

	let { code, label, wrap = false }: Props = $props();

	let copied = $state(false);
	let timer: ReturnType<typeof setTimeout> | undefined;

	async function copy(): Promise<void> {
		try {
			await navigator.clipboard.writeText(code);
			copied = true;
			clearTimeout(timer);
			timer = setTimeout(() => (copied = false), 2000);
		} catch {
			copied = false;
		}
	}
</script>

<div class="snippet">
	<pre class:wrap><code>{code}</code></pre>
	<button class="btn btn-sm copy" type="button" onclick={copy} aria-label="Copy {label}">
		{copied ? 'Copied' : 'Copy'}
	</button>
	<span class="visually-hidden" aria-live="polite">{copied ? `${label} copied` : ''}</span>
</div>

<style>
	.snippet {
		position: relative;
		min-width: 0;
	}
	pre {
		margin: 0;
		padding: var(--space-3) var(--space-4);
		padding-right: calc(var(--space-4) + 4.5rem);
		overflow-x: auto;
		background: var(--color-bg-subtle);
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
		font-family: var(--font-mono);
		font-size: var(--text-sm);
		line-height: 1.55;
		color: var(--color-text);
	}
	pre.wrap {
		white-space: pre-wrap;
		overflow-wrap: anywhere;
	}
	.copy {
		position: absolute;
		top: var(--space-2);
		right: var(--space-2);
	}
</style>
