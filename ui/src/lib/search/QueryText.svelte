<script lang="ts">
	import { queryTokens } from './query-tokens.ts';

	interface Props {
		query: string;
	}

	let { query }: Props = $props();

	const tokens = $derived(queryTokens(query));
</script>

<!-- A query with the search box's syntax colours (display only). -->
<code class="query-text">{#each tokens as t, i (i)}{#if t.kind === 'text' || t.kind === 'space'}{t.text}{:else}<span class="q-{t.kind}">{t.text}</span>{/if}{/each}</code>

<style>
	.query-text {
		font-family: var(--font-mono);
		font-size: inherit;
		color: var(--color-text);
		white-space: pre;
	}
	.q-field {
		color: var(--syntax-property);
	}
	.q-negation,
	.q-keyword {
		color: var(--syntax-keyword);
		font-weight: var(--weight-semibold);
	}
	.q-quoted {
		color: var(--syntax-string);
	}
	.q-paren {
		color: var(--syntax-punctuation);
		font-weight: var(--weight-semibold);
	}
</style>
