<script lang="ts">
	import { tick } from 'svelte';
	import Icon from '#lib/components/Icon.svelte';
	import { graphql } from '#lib/graphql.ts';
	import type { CompareBranchesQuery, CompareBranchesQueryVariables } from '#lib/graphql-types.ts';
	import { COMPARE_BRANCHES } from './queries.ts';

	let {
		repo,
		label,
		placeholder,
		value = $bindable()
	}: {
		repo: string;
		label: string;
		placeholder: string;
		value: string;
	} = $props();

	const id = $props.id();
	let input: HTMLInputElement;
	let open = $state(false);
	let query = $state('');
	let names = $state.raw<string[]>([]);
	let more = $state(false);
	let loading = $state(false);
	let failed = $state(false);
	let active = $state(-1);

	$effect(() => {
		if (!open) return;
		const variables = { repo, query: query.trim() };
		const controller = new AbortController();
		active = -1;
		names = [];
		more = false;
		loading = true;
		failed = false;
		const timer = setTimeout(
			async () => {
				try {
					const data = await graphql<CompareBranchesQuery, CompareBranchesQueryVariables>(COMPARE_BRANCHES, variables, {
						signal: controller.signal
					});
					if (controller.signal.aborted) return;
					const refs = data.repository?.gitRefs;
					names = refs?.nodes.map((r) => r.displayName) ?? [];
					more = refs?.pageInfo.hasNextPage ?? false;
				} catch {
					if (!controller.signal.aborted) failed = true;
				} finally {
					if (!controller.signal.aborted) loading = false;
				}
			},
			variables.query ? 120 : 0
		);
		return () => {
			clearTimeout(timer);
			controller.abort();
		};
	});

	function browse() {
		if (open) return;
		query = '';
		open = true;
	}

	function select(name: string) {
		input.focus();
		value = name;
		open = false;
	}

	function keydown(event: KeyboardEvent) {
		if (event.isComposing) return;
		if (event.key === 'ArrowDown' || event.key === 'ArrowUp') {
			event.preventDefault();
			browse();
			if (!names.length) return;
			const last = names.length - 1;
			active = event.key === 'ArrowDown' ? Math.min(active + 1, last) : active < 0 ? last : Math.max(0, active - 1);
			void tick().then(() => document.getElementById(id + '-' + active)?.scrollIntoView({ block: 'nearest' }));
		} else if (open && event.key === 'Enter') {
			const name = names[active];
			if (name !== undefined) {
				event.preventDefault();
				select(name);
			} else open = false;
		} else if (open && event.key === 'Escape') {
			event.preventDefault();
			open = false;
		}
	}
</script>

<div
	class="revision"
	onfocusout={(event) => {
		if (!event.currentTarget.contains(event.relatedTarget as Node | null)) open = false;
	}}
>
	<label for={id}>{label}</label>
	<div class="control">
		<input
			{id}
			class="input mono"
			role="combobox"
			aria-expanded={open}
			aria-autocomplete="list"
			aria-controls={id + '-branches'}
			aria-activedescendant={open && active >= 0 ? id + '-' + active : undefined}
			autocomplete="off"
			spellcheck="false"
			{placeholder}
			bind:this={input}
			bind:value
			onfocus={browse}
			onclick={browse}
			oninput={(event) => {
				query = event.currentTarget.value;
				open = true;
			}}
			onkeydown={keydown}
		/>
		<button
			type="button"
			class="caret"
			tabindex="-1"
			aria-label={'Browse ' + label.toLowerCase() + ' branches'}
			onmousedown={(e) => e.preventDefault()}
			onclick={() => {
				if (open) open = false;
				else {
					input.focus();
					browse();
				}
			}}
		>
			<Icon name="chevron-down" size={16} />
		</button>
	</div>
	{#if open}
		<div class="menu card">
			<div class="caption">Branches</div>
			<div class="options" role="listbox" id={id + '-branches'} aria-label={label + ' branches'} aria-busy={loading}>
				{#each names as name, i (name)}
					<button
						id={id + '-' + i}
						type="button"
						class="option mono"
						class:active={i === active}
						role="option"
						aria-selected={i === active}
						tabindex="-1"
						onmousedown={(e) => e.preventDefault()}
						onclick={() => select(name)}
					>
						<Icon name="branch" size={14} /><span>{name}</span>
						{#if name === value}<Icon name="check" size={14} />{/if}
					</button>
				{/each}
			</div>
			{#if loading}<p role="status">Loading branches…</p>
			{:else if failed}<p role="status">Couldn’t load branches. You can still enter a revision.</p>
			{:else if names.length === 0}<p role="status">No matching branches. You can still compare this revision.</p>
			{:else if more}<p>Type to narrow the first {names.length} matches.</p>{/if}
		</div>
	{/if}
</div>

<style>
	.revision {
		position: relative;
		flex: 1;
		min-width: 0;
	}
	label,
	.caption {
		display: block;
		color: var(--color-text-muted);
		font-size: var(--text-xs);
		font-weight: var(--weight-semibold);
	}
	label {
		margin-bottom: var(--space-1);
	}
	.control {
		position: relative;
	}
	.control input {
		width: 100%;
		padding-right: 2rem;
	}
	.caret {
		position: absolute;
		inset: 0 0 0 auto;
		width: 2rem;
		display: grid;
		place-items: center;
		background: none;
		border: 0;
		color: var(--color-text-muted);
		cursor: pointer;
	}
	.menu {
		position: absolute;
		top: calc(100% + 4px);
		z-index: 20;
		width: 100%;
		padding: var(--space-1);
		box-shadow: var(--shadow-lg);
	}
	.caption,
	.menu p {
		padding: var(--space-1) var(--space-2);
	}
	.menu p {
		font-size: var(--text-xs);
		color: var(--color-text-muted);
		margin: 0;
	}
	.options {
		max-height: 16rem;
		overflow: auto;
	}
	.option {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		padding: var(--space-1) var(--space-2);
		width: 100%;
		background: none;
		border: none;
		color: inherit;
		text-align: left;
		cursor: pointer;
		border-radius: 4px;
	}
	.option span {
		flex: 1;
		min-width: 0;
		overflow-wrap: anywhere;
	}
	.option.active,
	.option:hover {
		background: var(--color-bg-muted);
	}
</style>
