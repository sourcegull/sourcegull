<!--
	Edits one subject's settings document, as forms (the default) or as JSON with comments. Both
	edit the same text: a form changes only the value it touches, so comments and keys it does not
	show stay as they are.
-->
<script lang="ts">
	import { formatDateTime } from '#lib/format.svelte.ts';
	import { beforeNavigate } from '$app/navigation';
	import { onMount } from 'svelte';
	import JsoncEditor from '#lib/components/JsoncEditor.svelte';
	import SaveBar from '#lib/components/SaveBar.svelte';
	import ConfigField from '#lib/components/site-config/ConfigField.svelte';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import {
		DEFAULT_SETTINGS,
		EMPTY_SETTINGS,
		OVERWRITE_SETTINGS,
		type DefaultSettingsResult,
		type SettingsVersion
	} from '#lib/orgs/queries.ts';
	import { parseJsonc, setAt } from '#lib/jsonc-edit.ts';
	import { refreshViewerSettings } from '#lib/settings/viewer-settings.ts';
	import { settingsForm, type SettingsSchema, type SettingsScope } from '#lib/settings/form.ts';
	import { SETTINGS_REFERENCE_PATH, SETTINGS_SCHEMA_URL } from '#lib/settings/reference.ts';
	import { requiredIssues, type ConfigPath } from '#lib/site-config-form.ts';
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
		/** Which settings apply to the subject. */
		scope: SettingsScope;
		/** The higher level of the cascade, whose values apply where this document sets none. */
		inherited?: Record<string, unknown> | null;
		/** Where an inherited value comes from, shown next to it. */
		inheritedNote?: string;
	}

	let { subject, latest, onsaved, label, readOnly = false, scope, inherited = null, inheritedNote }: Props = $props();

	let contents = $state('');
	let baseline = $state('');
	let saving = $state(false);
	let error = $state<string | null>(null);
	let conflict = $state(false);

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

	/** The version the editor shows; undefined until the first one arrives. */
	let shownVersion: number | null | undefined = undefined;

	// Reset the editor when another version arrives from the server, not when the same one is
	// loaded again, which would drop unsaved edits. A subject without settings starts from the
	// commented template of every setting.
	$effect(() => {
		const version = latest?.id ?? null;
		if (version === shownVersion) return;
		shownVersion = version;
		const text = latest?.contents ?? template ?? EMPTY_SETTINGS;
		contents = text;
		baseline = text;
		showRequired = false;
		if (!latest && !templateRequested) void loadTemplate();
	});

	const dirty = $derived(contents !== baseline);
	let savedAt = $state<number | null>(null);
	const SAVE_NOTES: Record<SettingsScope, string> = {
		global: 'Saved settings apply to every user.',
		organization: 'Saved settings apply to every member of this organization.',
		user: 'Saved settings apply only to you.'
	};

	function discard(): void {
		contents = baseline;
		error = null;
		conflict = false;
		showRequired = false;
	}

	let schema = $state.raw<SettingsSchema | null>(null);
	let view = $state<'settings' | 'json'>('settings');
	let schemaFailed = $state(false);
	/** Whether a save was refused for empty required values; they are marked from then on. */
	let showRequired = $state(false);

	/** The document being edited, parsed (null while it has a syntax error). */
	const parsed = $derived(parseJsonc(contents));
	const savedDoc = $derived(parseJsonc(baseline));
	const sections = $derived(schema && parsed ? settingsForm(schema, parsed, scope, inherited) : []);
	const missing = $derived(
		parsed
			? requiredIssues(
					sections.flatMap((s) => s.fields),
					parsed
				)
			: []
	);
	const issues = $derived(showRequired ? missing : []);

	function setValue(path: ConfigPath, value: unknown): void {
		contents = setAt(contents, path, value);
	}

	onMount(async () => {
		try {
			const resp = await fetch(SETTINGS_SCHEMA_URL);
			if (!resp.ok) throw new Error(resp.statusText);
			schema = (await resp.json()) as SettingsSchema;
		} catch {
			// Without the schema there are no forms; the JSON editor still works.
			schemaFailed = true;
			view = 'json';
		}
	});

	beforeNavigate(({ cancel, willUnload }) => {
		if (!dirty || readOnly) return;
		// For full page unloads the browser shows its own prompt when we cancel.
		if (willUnload || !confirm('Discard unsaved changes to these settings?')) cancel();
	});

	async function save(): Promise<void> {
		if (saving || !dirty || readOnly) return;
		showRequired = true;
		if (missing.length > 0) {
			view = 'settings';
			error = null;
			return;
		}
		saving = true;
		error = null;
		conflict = false;
		try {
			await graphql(OVERWRITE_SETTINGS, { subject, lastID: latest?.id ?? null, contents });
			await onsaved();
			void refreshViewerSettings();
			savedAt = Date.now();
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
			>Last saved {formatDateTime(latest.createdAt)}{#if latest.author}{' '}by
				{latest.author.username}{/if}</span
		>
	{:else}
		<span class="muted">No settings saved yet.</span>
	{/if}
	<a href={SETTINGS_REFERENCE_PATH} target="_blank" rel="noopener">Settings reference</a>
</div>

<div class="tabs view-tabs" role="tablist" aria-label="Editor">
	<button
		class="tab"
		type="button"
		role="tab"
		id="settings-tab-form"
		aria-selected={view === 'settings'}
		aria-controls="settings-panel"
		disabled={schemaFailed}
		onclick={() => (view = 'settings')}
	>
		Settings
	</button>
	<button
		class="tab"
		type="button"
		role="tab"
		id="settings-tab-json"
		aria-selected={view === 'json'}
		aria-controls="settings-panel"
		onclick={() => (view = 'json')}
	>
		Edit as JSON
	</button>
</div>

<div
	id="settings-panel"
	role="tabpanel"
	aria-labelledby={view === 'settings' ? 'settings-tab-form' : 'settings-tab-json'}
>
	{#if view === 'json'}
		<p class="muted json-hint">
			JSON with comments{#if !readOnly}; press <kbd>Ctrl</kbd>/<kbd>⌘</kbd>+<kbd>S</kbd> to save{/if}.
		</p>
		<JsoncEditor bind:value={contents} {label} {readOnly} onsave={save} />
	{:else if parsed === null}
		<div class="alert alert-warning" role="status">
			The document has a syntax error, so the settings can't be shown.
			<button class="btn btn-sm" type="button" onclick={() => (view = 'json')}>Fix it in the JSON editor</button>
		</div>
	{:else if !schema}
		<p class="loading" aria-live="polite"><span class="spinner"></span> Loading settings…</p>
	{:else}
		{#each sections as { section, fields } (section.id)}
			<section class="card settings-section" aria-labelledby="settings-section-{section.id}">
				<h3 id="settings-section-{section.id}">{section.title}</h3>
				<p class="muted section-description">{section.description}</p>
				{#each fields as field (field.key)}
					<ConfigField
						{field}
						doc={parsed}
						saved={savedDoc}
						{issues}
						disabled={readOnly}
						onchange={setValue}
						defaultNote={inherited?.[field.key] !== undefined ? inheritedNote : undefined}
					/>
				{/each}
			</section>
		{/each}
	{/if}
</div>

{#if !readOnly}
	<SaveBar
		{dirty}
		{saving}
		saveDisabled={conflict}
		note={SAVE_NOTES[scope]}
		{savedAt}
		ondiscard={discard}
		onsave={save}
	>
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
		{#if issues.length > 0}
			<div class="alert alert-danger" role="alert">
				<strong>Not saved.</strong> Fill in the required values marked in the form.
			</div>
		{:else if error}
			<div class="alert alert-danger" role="alert"><strong>Not saved.</strong> {error}</div>
		{/if}
	</SaveBar>
{/if}

<style>
	.settings-toolbar {
		margin-bottom: var(--space-2);
	}
	.view-tabs {
		margin-bottom: var(--space-3);
	}
	.json-hint {
		margin: 0 0 var(--space-2);
	}
	.settings-section {
		margin-bottom: var(--space-4);
	}
	.settings-section h3 {
		margin: 0;
	}
	.section-description {
		margin: var(--space-1) 0 var(--space-2);
	}
</style>
