<!-- The signed-in user's own settings, the last level of the settings cascade. -->
<script lang="ts">
	import { onMount } from 'svelte';
	import SettingsEditor from '#lib/components/SettingsEditor.svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import { parseJsonc } from '#lib/jsonc-edit.ts';
	import { USER_SETTINGS, type UserSettingsResult } from '#lib/orgs/queries.ts';
	import { mergeSettings } from '#lib/settings/viewer.ts';

	let user = $state.raw<UserSettingsResult['currentUser']>(null);
	let error = $state<string | null>(null);

	/** What the global and organization settings set for this user. */
	const inherited = $derived(
		mergeSettings(
			(user?.settingsCascade.subjects ?? [])
				.filter((s) => s.__typename !== 'User')
				.map((s) => parseJsonc(s.latestSettings?.contents ?? ''))
		)
	);

	async function load(): Promise<void> {
		try {
			user = (await graphql<UserSettingsResult>(USER_SETTINGS)).currentUser;
			error = null;
		} catch (err) {
			error = errorMessage(err);
		}
	}

	onMount(() => {
		void load();
	});
</script>

<svelte:head><title>Preferences · Settings · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Preferences</h1>
		<p class="page-description">Your own settings, over the global settings and those of your organizations.</p>
	</div>
</div>

{#if error}<div class="alert alert-danger" role="alert">{error}</div>{/if}
{#if user}
	<SettingsEditor
		subject={user.id}
		latest={user.latestSettings}
		onsaved={load}
		label="Your settings (JSON with comments)"
		scope="user"
		{inherited}
		inheritedNote="Inherited from the global or organization settings."
	/>
{:else if !error}
	<p class="loading"><span class="spinner"></span> Loading…</p>
{/if}
