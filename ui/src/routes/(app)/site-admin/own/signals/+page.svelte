<script lang="ts">
	import type { OwnSignalConfigurationsQuery, UpdateOwnSignalConfigurationsMutation } from '#lib/graphql-types.ts';
	import { onMount } from 'svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		OWN_SIGNAL_CONFIGURATIONS,
		UPDATE_OWN_SIGNAL_CONFIGURATIONS,
		parsePatterns,
		type OwnSignalConfiguration
	} from '#lib/own/queries.ts';

	// Ownership signal configuration (Code Ownership): each signal (recent contributors,
	// recent views, analytics) is turned on here and can skip repositories by name patterns.

	type Vars = Record<string, unknown>;

	interface Draft {
		name: string;
		description: string;
		enabled: boolean;
		patterns: string;
	}

	const TITLES: Record<string, string> = {
		'recent-contributors': 'Recent contributors',
		'recent-views': 'Recent views',
		analytics: 'Ownership analytics'
	};

	let drafts = $state<Draft[]>([]);
	let loading = $state(true);
	let saving = $state(false);
	let error = $state<string | null>(null);
	let saved = $state(false);

	function toDrafts(configs: OwnSignalConfiguration[]): Draft[] {
		return configs.map((c) => ({
			name: c.name,
			description: c.description,
			enabled: c.isEnabled,
			patterns: c.excludedRepoPatterns.join('\n')
		}));
	}

	async function load(): Promise<void> {
		loading = true;
		error = null;
		try {
			const r = await graphql<OwnSignalConfigurationsQuery>(OWN_SIGNAL_CONFIGURATIONS);
			drafts = toDrafts(r.ownSignalConfigurations);
		} catch (err) {
			error = errorMessage(err);
		} finally {
			loading = false;
		}
	}

	async function save(e: SubmitEvent): Promise<void> {
		e.preventDefault();
		saving = true;
		saved = false;
		error = null;
		try {
			const configs = drafts.map((d) => ({
				name: d.name,
				enabled: d.enabled,
				excludedRepoPatterns: parsePatterns(d.patterns)
			}));
			const r = await graphql<UpdateOwnSignalConfigurationsMutation, Vars>(
				UPDATE_OWN_SIGNAL_CONFIGURATIONS,
				{ input: { configs } }
			);
			drafts = toDrafts(r.updateOwnSignalConfigurations);
			saved = true;
		} catch (err) {
			error = errorMessage(err);
		} finally {
			saving = false;
		}
	}

	onMount(() => {
		void load();
	});
</script>

<svelte:head><title>Ownership signals · Site admin · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Ownership signals</h1>
		<p class="page-description">
			Signals suggest owners where CODEOWNERS files and assignments say nothing: people who committed to
			a file in the last 90 days, and people who viewed it. They are computed in the background and
			listed after the owners. Exclude repositories with name patterns, one per line
			(<span class="code-inline">*</span> matches any characters).
		</p>
	</div>
</div>

{#if error}
	<div class="alert alert-danger" role="alert">{error}</div>
{/if}
{#if saved}
	<div class="alert alert-success" role="status">Saved. Changes apply from the next indexing run.</div>
{/if}

<form class="signals" onsubmit={save} aria-busy={loading}>
	{#each drafts as d (d.name)}
		<fieldset class="card">
			<legend class="visually-hidden">{TITLES[d.name] ?? d.name}</legend>
			<div class="card-header">
				<label class="toggle">
					<input type="checkbox" bind:checked={d.enabled} disabled={saving} />
					<span>{TITLES[d.name] ?? d.name}</span>
				</label>
				<span class="badge" class:badge-success={d.enabled}>{d.enabled ? 'On' : 'Off'}</span>
			</div>
			<p class="muted">{d.description}</p>
			<label class="field patterns">
				<span class="label">Excluded repositories</span>
				<textarea
					class="input mono"
					rows="3"
					bind:value={d.patterns}
					disabled={saving}
					placeholder="gitlab.example.com/sandbox/*"
				></textarea>
			</label>
		</fieldset>
	{:else}
		{#if loading}
			<p class="loading"><span class="spinner"></span> Loading…</p>
		{:else}
			<div class="empty-state"><p>No signals.</p></div>
		{/if}
	{/each}
	{#if drafts.length > 0}
		<div>
			<button class="btn btn-primary" type="submit" disabled={saving}>Save</button>
		</div>
	{/if}
</form>

<style>
	.signals {
		display: flex;
		flex-direction: column;
		gap: var(--space-4);
		max-width: 48rem;
	}
	fieldset.card {
		margin: 0;
		min-width: 0;
	}
	.toggle {
		display: inline-flex;
		align-items: center;
		gap: var(--space-2);
		font-weight: var(--weight-semibold);
	}
	fieldset p {
		margin: 0 0 var(--space-3);
	}
	.patterns {
		margin: 0;
	}
	.patterns textarea {
		width: 100%;
		resize: vertical;
	}
</style>
