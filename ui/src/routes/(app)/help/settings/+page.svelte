<script lang="ts">
	import { onMount } from 'svelte';
	import {
		SETTINGS_SCHEMA_URL,
		settingsReference,
		type SchemaNode,
		type SettingRow
	} from '#lib/settings/reference.ts';

	let settings = $state.raw<SettingRow[]>([]);
	let error = $state<string | null>(null);
	let loading = $state(true);

	onMount(async () => {
		try {
			const resp = await fetch(SETTINGS_SCHEMA_URL);
			if (!resp.ok) throw new Error(`${resp.status} ${resp.statusText}`);
			settings = settingsReference((await resp.json()) as SchemaNode);
		} catch (err) {
			error = `Loading the settings schema failed: ${err instanceof Error ? err.message : String(err)}`;
		} finally {
			loading = false;
		}
	});
</script>

<svelte:head><title>Settings reference · Sourcegull</title></svelte:head>

{#snippet details(row: SettingRow)}
	{#if row.description}<p class="desc">{row.description}</p>{/if}
	{#if row.values.length}
		<ul class="values">
			{#each row.values as v (v.value)}
				<li><code>"{v.value}"</code>{#if v.description}: {v.description}{/if}</li>
			{/each}
		</ul>
	{/if}
	{#if row.fields.length}
		<ul class="values">
			{#each row.fields as f (f.key)}
				<li>
					<code>{f.key}</code> <span class="muted">({f.type})</span>{#if f.description}: {f.description}{/if}
					{#if f.values.length}
						<span class="muted">One of {f.values.map((v) => `"${v.value}"`).join(', ')}.</span>
					{/if}
				</li>
			{/each}
		</ul>
	{/if}
{/snippet}

<div class="page-header"><h1>Settings reference</h1></div>
<p class="muted">
	Settings are JSON with comments. They apply in a cascade: global settings (Site admin), then each
	organization's, then the user's own; later ones override earlier ones key by key. Keys not listed
	here are kept as they are and passed to the UI. The JSON Schema is at
	<a href={SETTINGS_SCHEMA_URL} target="_blank" rel="noopener">{SETTINGS_SCHEMA_URL}</a>.
</p>

{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}

<div class="table-scroll">
	<table class="table" aria-busy={loading}>
		<thead>
			<tr>
				<th scope="col">Key</th>
				<th scope="col">Type</th>
				<th scope="col">Default</th>
				<th scope="col">Description</th>
			</tr>
		</thead>
		<tbody>
			{#each settings as row (row.key)}
				<tr id={row.key}>
					<td><code>{row.key}</code></td>
					<td>{row.type}</td>
					<td>{#if row.default !== null}<code>{row.default}</code>{:else}<span class="muted">—</span>{/if}</td>
					<td>{@render details(row)}</td>
				</tr>
			{:else}
				<tr><td colspan="4" class="muted">{loading ? 'Loading…' : 'No settings documented.'}</td></tr>
			{/each}
		</tbody>
	</table>
</div>

<style>
	.desc {
		margin: 0;
	}
	.values {
		margin: var(--space-1) 0 0;
		padding-left: 1.25em;
	}
	td {
		vertical-align: top;
	}
</style>
