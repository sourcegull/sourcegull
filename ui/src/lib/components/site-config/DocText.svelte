<script lang="ts">
	import { codeSpans } from '#lib/site-config-form.ts';

	/** Schema doc text; `backtick` spans render as inline code. */
	let { text }: { text: string } = $props();
</script>

{#each codeSpans(text) as part, i (i)}{#if part.code}<code class="code-inline">{part.text}</code>{:else}{part.text}{/if}{/each}
