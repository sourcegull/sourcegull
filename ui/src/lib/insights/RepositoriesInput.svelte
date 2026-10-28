<!--
	A list of repositories picked by name (trend creation pages): the chosen ones as removable
	chips, and an input that suggests repository names as you type. Enter, a comma or leaving
	the field adds what was typed.
-->
<script lang="ts">
	import { graphql } from '#lib/graphql.ts';
	import { REPOSITORY_NAMES, type RepositoryNamesResult } from '#lib/search/queries.ts';

	interface Props {
		value: string[];
		id: string;
		invalid?: boolean;
		describedby?: string;
		placeholder?: string;
		onblur?: () => void;
	}

	let {
		value = $bindable(),
		id,
		invalid = false,
		describedby,
		placeholder = 'Search repositories...',
		onblur
	}: Props = $props();

	let text = $state('');
	let suggestions = $state<string[]>([]);
	let timer: ReturnType<typeof setTimeout> | undefined;
	let seq = 0;

	function add(raw: string): void {
		const names = raw
			.split(/[\s,]+/)
			.map((n) => n.trim())
			.filter(Boolean);
		if (names.length === 0) return;
		value = [...value, ...names.filter((n) => !value.includes(n))];
		text = '';
		suggestions = [];
	}

	function remove(name: string): void {
		value = value.filter((n) => n !== name);
	}

	function suggest(): void {
		clearTimeout(timer);
		const q = text.trim();
		if (!q) {
			suggestions = [];
			return;
		}
		const mine = ++seq;
		timer = setTimeout(async () => {
			try {
				const r = await graphql<RepositoryNamesResult, { query: string; first: number }>(REPOSITORY_NAMES, {
					query: q,
					first: 10
				});
				if (mine === seq) suggestions = r.repositories.nodes.map((n) => n.name).filter((n) => !value.includes(n));
			} catch {
				if (mine === seq) suggestions = [];
			}
		}, 200);
	}

	function keydown(e: KeyboardEvent): void {
		if (e.key === 'Enter' || e.key === ',') {
			if (text.trim()) {
				e.preventDefault();
				add(text);
			}
		} else if (e.key === 'Backspace' && text === '' && value.length > 0) {
			value = value.slice(0, -1);
		}
	}
</script>

<div class="repositories" class:invalid>
	{#if value.length > 0}
		<ul class="chips" aria-label="Chosen repositories">
			{#each value as name (name)}
				<li class="chip">
					<span class="mono">{name}</span>
					<button type="button" class="remove" aria-label={`Remove ${name}`} onclick={() => remove(name)}>×</button>
				</li>
			{/each}
		</ul>
	{/if}
	<input
		{id}
		class="input"
		list={`${id}-suggestions`}
		autocomplete="off"
		{placeholder}
		aria-invalid={invalid ? 'true' : undefined}
		aria-describedby={describedby}
		bind:value={text}
		oninput={suggest}
		onkeydown={keydown}
		onchange={() => {
			// Picking a suggestion from the list fires `change` with the full name.
			if (suggestions.includes(text.trim())) add(text);
		}}
		onblur={() => {
			add(text);
			onblur?.();
		}}
	/>
	<datalist id={`${id}-suggestions`}>
		{#each suggestions as s (s)}<option value={s}></option>{/each}
	</datalist>
</div>

<style>
	.repositories {
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
	}
	.chips {
		list-style: none;
		margin: 0;
		padding: 0;
		display: flex;
		flex-wrap: wrap;
		gap: var(--space-1);
	}
	.chip {
		display: inline-flex;
		align-items: center;
		gap: var(--space-1);
		max-width: 100%;
		padding: 2px var(--space-2);
		border: 1px solid var(--color-border);
		border-radius: 999px;
		background: var(--color-bg-subtle);
		font-size: 0.85rem;
	}
	.chip .mono {
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
	}
	.remove {
		border: 0;
		background: none;
		color: var(--color-text-muted);
		cursor: pointer;
		font-size: 1rem;
		line-height: 1;
		padding: 0 2px;
	}
	.remove:hover {
		color: var(--color-danger);
	}
	.invalid .input {
		border-color: var(--color-danger);
	}
</style>
