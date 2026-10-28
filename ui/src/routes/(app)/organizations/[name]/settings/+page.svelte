<script lang="ts">
	import { page } from '$app/state';
	import { untrack } from 'svelte';
	import SettingsEditor from '#lib/components/SettingsEditor.svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		ORGANIZATION_SETTINGS,
		UPDATE_ORGANIZATION,
		formatFinalSettings,
		type OrganizationSettingsResult,
		type Vars
} from '#lib/orgs/queries.ts';

	const name = $derived(page.params.name ?? '');
	let org = $state<OrganizationSettingsResult['organization']>(null);
	let displayName = $state('');
	let error = $state<string | null>(null);
	let notice = $state<string | null>(null);
	let saving = $state(false);
	let showMerged = $state(false);

	async function load(): Promise<void> {
		try {
			org = (await graphql<OrganizationSettingsResult, Vars>(ORGANIZATION_SETTINGS, { name })).organization;
			displayName = org?.displayName ?? '';
			error = null;
		} catch (err) {
			error = errorMessage(err);
		}
	}

	$effect(() => {
		void name;
		untrack(() => void load());
	});

	async function saveProfile(): Promise<void> {
		if (!org) return;
		saving = true;
		error = null;
		notice = null;
		try {
			await graphql(UPDATE_ORGANIZATION, { id: org.id, displayName: displayName.trim() });
			notice = 'Saved. Reload to see the new name in the sidebar.';
			await load();
		} catch (err) {
			error = errorMessage(err);
		} finally {
			saving = false;
		}
	}
</script>

<svelte:head><title>Settings · {name} · Sourcegull</title></svelte:head>

<div class="page-header"><h1>Settings</h1></div>
{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}
{#if notice}<div class="alert alert-success" role="status">{notice}</div>{/if}

{#if org}
	<section class="card">
		<h2>Profile</h2>
		<form
			class="toolbar profile"
			onsubmit={(event) => {
				event.preventDefault();
				void saveProfile();
			}}
		>
			<label for="org-display-name">Display name</label>
			<input id="org-display-name" class="input" bind:value={displayName} placeholder={name} />
			<button class="btn btn-primary" type="submit" disabled={saving}>Save</button>
		</form>
	</section>

	<section>
		<h2>Organization settings</h2>
		<p class="muted">
			Apply to every member, over the global settings and under each member's own settings. JSON with
			comments; press <kbd>Ctrl</kbd>/<kbd>⌘</kbd>+<kbd>S</kbd> to save.
		</p>
		<SettingsEditor
			subject={org.id}
			latest={org.latestSettings}
			onsaved={load}
			label="Organization settings (JSON with comments)"
		/>
		<button class="btn btn-link" type="button" onclick={() => (showMerged = !showMerged)}>
			{showMerged ? 'Hide' : 'Show'} merged settings (global + organization)
		</button>
		{#if showMerged}<pre class="merged">{formatFinalSettings(org.settingsCascade.final)}</pre>{/if}
	</section>
{:else if !error}
	<p class="muted">Loading…</p>
{/if}

<style>
	.profile {
		flex-wrap: wrap;
	}
	section {
		margin-bottom: var(--space-6);
	}
	.merged {
		background: var(--color-bg-subtle);
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
		padding: var(--space-3);
		overflow: auto;
	}
</style>
