<!--
	The global settings editor. Settings from SOURCEGULL_GLOBAL_SETTINGS_FILE are shown read-only.
-->
<script lang="ts">
	import { onMount } from 'svelte';
	import SettingsEditor from '#lib/components/SettingsEditor.svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import { GLOBAL_SETTINGS, type GlobalSettingsResult } from '#lib/orgs/queries.ts';

	let site = $state<GlobalSettingsResult['site'] | null>(null);
	let error = $state<string | null>(null);

	const readOnly = $derived(site ? (site.latestSettings?.readOnly ?? false) || !site.viewerCanAdminister : false);
	const filePath = $derived(site?.declaredConfigFiles.find((f) => f.source === 'global_settings')?.path ?? null);

	async function load(): Promise<void> {
		try {
			site = (await graphql<GlobalSettingsResult>(GLOBAL_SETTINGS)).site;
			error = null;
		} catch (err) {
			error = errorMessage(err);
		}
	}

	onMount(() => {
		void load();
	});
</script>

<svelte:head><title>Global settings · Site admin · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Global settings</h1>
		<p class="page-description">
			Settings for every user, overridden by organization and user settings (the settings cascade).
			JSON with comments{#if !readOnly}; press <kbd>Ctrl</kbd>/<kbd>⌘</kbd>+<kbd>S</kbd> to save{/if}.
		</p>
	</div>
</div>

{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}
{#if readOnly}
	<div class="alert alert-info" role="status" data-testid="global-settings-read-only">
		These settings are read from SOURCEGULL_GLOBAL_SETTINGS_FILE{#if filePath}
			(<code class="code-inline">{filePath}</code>){/if} on the server, so they can't be changed here. Edit the
		file instead.
	</div>
{/if}
{#if site}
	{#key readOnly}
		<SettingsEditor subject={site.id} latest={site.latestSettings} onsaved={load} label="Global settings" {readOnly} />
	{/key}
{:else if !error}
	<p class="loading"><span class="spinner"></span> Loading…</p>
{/if}
