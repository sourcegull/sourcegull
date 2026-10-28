<script lang="ts">
	import JsoncEditor from '#lib/components/JsoncEditor.svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		DEFAULT_SETTINGS,
		EMPTY_SETTINGS,
		OVERWRITE_SETTINGS,
		type DefaultSettingsResult,
		type SettingsVersion
	} from '#lib/orgs/queries.ts';
	import { SETTINGS_REFERENCE_PATH } from '#lib/settings/reference.ts';
	import { isConflictError } from '#lib/site-config.ts';

	interface Props {
		/** The settings subject (site, organization or user node ID). */
		subject: string;
		/** The latest saved version, or null when the subject has none. */
		latest: SettingsVersion | null;
		/** Reloads `latest` after a save. */
		onsaved: () => Promise<void>;
		label: string;
		readOnly?: boolean;
	}

	let { subject, latest, onsaved, label, readOnly = false }: Props = $props();

	let contents = $state('');
	let baseline = $state('');
	let saving = $state(false);
	let error = $state<string | null>(null);
	let conflict = $state(false);
	let notice = $state<string | null>(null);

	/** The commented template of every setting, for subjects without settings. */
	let template: string | null = null;
	let templateRequested = false;

	/** Loads the template once; shows it unless the user started typing or settings arrived. */
	async function loadTemplate(): Promise<void> {
		templateRequested = true;
		try {
			template = (await graphql<DefaultSettingsResult>(DEFAULT_SETTINGS)).site.defaultSettings;
		} catch {
			return;
		}
		if (!latest && contents === baseline) {
			contents = template;
			baseline = template;
		}
	}

	// Reset the editor whenever a new version arrives from the server; a subject without
	// settings starts from the commented template of every setting.
	$effect(() => {
		const text = latest?.contents ?? template ?? EMPTY_SETTINGS;
		contents = text;
		baseline = text;
		if (!latest && !templateRequested) void loadTemplate();
	});

	const dirty = $derived(contents !== baseline);

	async function save(): Promise<void> {
		if (saving || !dirty || readOnly) return;
		saving = true;
		error = null;
		notice = null;
		conflict = false;
		try {
			await graphql(OVERWRITE_SETTINGS, { subject, lastID: latest?.id ?? null, contents });
			await onsaved();
			notice = 'Saved.';
		} catch (err) {
			if (isConflictError(err)) conflict = true;
			else error = errorMessage(err);
		} finally {
			saving = false;
		}
	}
</script>

<div class="toolbar settings-toolbar">
	{#if latest}
		<span class="muted"
			>Last saved {new Date(latest.createdAt).toLocaleString()}{#if latest.author}
				by {latest.author.username}{/if}</span
		>
	{:else}
		<span class="muted">No settings saved yet.</span>
	{/if}
	<a href={SETTINGS_REFERENCE_PATH} target="_blank" rel="noopener">Settings reference</a>
	<span class="spacer"></span>
	{#if dirty}<span class="badge" aria-live="polite">Unsaved changes</span>{/if}
	{#if !readOnly}
		<button class="btn" type="button" onclick={() => (contents = baseline)} disabled={!dirty || saving}
			>Discard</button
		>
		<button class="btn btn-primary" type="button" onclick={save} disabled={!dirty || saving || conflict}
			>{saving ? 'Saving…' : 'Save'}</button
		>
	{/if}
</div>

{#if conflict}
	<div class="alert alert-warning" role="alert">
		<strong>The settings were changed since you loaded them.</strong> Copy your edits, then reload.
		<div class="toolbar">
			<button
				class="btn btn-sm"
				type="button"
				onclick={async () => {
					conflict = false;
					await onsaved();
				}}>Reload latest version</button
			>
			<button class="btn btn-sm" type="button" onclick={() => navigator.clipboard?.writeText(contents)}
				>Copy my edits</button
			>
		</div>
	</div>
{/if}
{#if error}
	<div class="alert alert-danger" role="alert"><strong>Save failed.</strong> {error}</div>
{/if}
{#if notice}<div class="alert alert-success" role="status">{notice}</div>{/if}

<JsoncEditor bind:value={contents} {label} {readOnly} onsave={save} />

<style>
	.settings-toolbar {
		margin-bottom: var(--space-2);
	}
	.spacer {
		flex: 1;
	}
</style>
