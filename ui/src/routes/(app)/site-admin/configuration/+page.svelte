<script lang="ts">
	import { beforeNavigate } from '$app/navigation';
	import { EditorView } from '@codemirror/view';
	import { onMount, tick } from 'svelte';
	import DeclaredConfigFiles from '#lib/components/DeclaredConfigFiles.svelte';
	import JsoncEditor from '#lib/components/JsoncEditor.svelte';
	import AuthProvidersEditor from '#lib/components/site-config/AuthProvidersEditor.svelte';
	import ConfigField from '#lib/components/site-config/ConfigField.svelte';
	import type { DeclaredConfigFile } from '#lib/declared-config.ts';
	import { errorMessage, graphql } from '#lib/graphql.ts';
	import { parseJsonc, setAt } from '#lib/jsonc-edit.ts';
	import {
		ORGANIZE_SITE_CONFIGURATION,
		SITE_CONFIGURATION,
		UPDATE_SITE_CONFIGURATION,
		type OrganizeSiteConfigurationResult,
		type SiteConfigurationResult,
		type UpdateSiteConfigurationResult,
		type UpdateSiteConfigurationVariables
	} from '#lib/queries.ts';
	import { isConflictError } from '#lib/site-config.ts';
	import {
		SITE_CONFIG_SCHEMA_URL,
		sectionLine,
		sectionRows,
		sectionsOf,
		setCount,
		type ConfigSection,
		type SiteConfigSchema
	} from '#lib/site-config-sections.ts';
	import {
		issuesFromError,
		parseIssue,
		sectionFields,
		type ConfigIssue,
		type ConfigPath,
		type JsonSchema
	} from '#lib/site-config-form.ts';

	/** The configuration as last loaded from the server. */
	let loaded = $state<SiteConfigurationResult['site']['configuration'] | null>(null);
	/** The configuration-as-code files in use, shown above the editor. */
	let declaredFiles = $state.raw<DeclaredConfigFile[]>([]);
	let contents = $state('');
	let loadError = $state<string | null>(null);
	let saveError = $state<string | null>(null);
	let conflict = $state(false);
	let saving = $state(false);
	let notice = $state<string | null>(null);

	const dirty = $derived(loaded !== null && contents !== loaded.effectiveContents);
	const readOnly = $derived(loaded?.readOnly ?? false);

	let schema = $state<(SiteConfigSchema & JsonSchema) | null>(null);
	let schemaFailed = $state(false);
	let selectedId = $state<string | null>(null);
	let organizing = $state(false);
	let editorHost = $state<HTMLDivElement | undefined>();
	let view = $state<'settings' | 'json'>('settings');

	/** The document being edited, parsed (null while it has a syntax error). */
	const parsed = $derived(parseJsonc(contents));
	const savedDoc = $derived(loaded ? parseJsonc(loaded.effectiveContents) : null);
	const sections = $derived(schema ? sectionsOf(schema, parsed) : []);
	const selected = $derived(
		sections.find((s) => s.id === selectedId) ?? (view === 'settings' ? sections[0] : null) ?? null
	);
	const rows = $derived(schema && selected ? sectionRows(schema, selected, parsed) : []);
	const selectedInDocument = $derived(selected ? sectionLine(contents, selected) !== null : false);
	const fields = $derived(schema && selected ? sectionFields(schema, selected.keys) : []);

	/** Validation problems: those of the last failed save, else those of the saved configuration. */
	const issues = $derived<ConfigIssue[]>(
		saveError
			? issuesFromError(saveError)
			: (loaded?.validationMessages.map(parseIssue).filter((i) => i.path.length > 0) ?? [])
	);
	function sectionIssues(section: ConfigSection): number {
		return issues.filter((i) => section.keys.includes(String(i.path[0]))).length;
	}

	async function loadSchema(): Promise<void> {
		try {
			const resp = await fetch(SITE_CONFIG_SCHEMA_URL);
			if (resp.ok) schema = (await resp.json()) as SiteConfigSchema & JsonSchema;
			else schemaFailed = true;
		} catch {
			schemaFailed = true;
		}
		// Without the schema there are no forms; the JSON editor still works.
		if (schemaFailed) view = 'json';
	}

	/** Applies a form edit to the document text, touching only that value. */
	function setValue(path: ConfigPath, value: unknown): void {
		contents = setAt(contents, path, value);
		notice = null;
	}

	/** Selects `section` and, in the JSON view, scrolls the editor to where it starts. */
	function reveal(section: ConfigSection): void {
		selectedId = section.id;
		if (view !== 'json') return;
		const line = sectionLine(contents, section);
		const dom = editorHost?.querySelector<HTMLElement>('.cm-editor');
		const editor = dom ? EditorView.findFromDOM(dom) : null;
		if (line === null || !editor) return;
		const pos = editor.state.doc.line(Math.min(line, editor.state.doc.lines)).from;
		editor.dispatch({
			selection: { anchor: pos },
			effects: EditorView.scrollIntoView(pos, { y: 'start', yMargin: 8 })
		});
		editor.focus();
	}

	async function showView(next: 'settings' | 'json'): Promise<void> {
		view = next;
		if (next === 'json' && selected) {
			await tick();
			reveal(selected);
		}
	}

	/** Reorders the document by section on the server; the result is an unsaved edit. */
	async function organize(): Promise<void> {
		if (organizing) return;
		organizing = true;
		saveError = null;
		notice = null;
		try {
			const data = await graphql<OrganizeSiteConfigurationResult, { input: string }>(
				ORGANIZE_SITE_CONFIGURATION,
				{
					input: contents
				}
			);
			const next = data.site.organizeConfiguration;
			notice =
				next === contents
					? 'The configuration is already in section order.'
					: 'Keys are now in section order, with their comments. Review the changes and save.';
			contents = next;
		} catch (err) {
			saveError = `Could not organize the configuration: ${errorMessage(err)}`;
		} finally {
			organizing = false;
		}
	}

	async function load(): Promise<void> {
		loadError = null;
		try {
			const data = await graphql<SiteConfigurationResult>(SITE_CONFIGURATION);
			loaded = data.site.configuration;
			declaredFiles = data.site.declaredConfigFiles;
			contents = loaded.effectiveContents;
			conflict = false;
			saveError = null;
		} catch (err) {
			loadError = errorMessage(err);
		}
	}

	async function save(): Promise<void> {
		if (!loaded || saving || !dirty || readOnly) return;
		saving = true;
		saveError = null;
		notice = null;
		conflict = false;
		try {
			const result = await graphql<UpdateSiteConfigurationResult, UpdateSiteConfigurationVariables>(
				UPDATE_SITE_CONFIGURATION,
				{ lastID: loaded.id, input: contents }
			);
			await load();
			notice = result.updateSiteConfiguration
				? 'Saved. Restart the server for some changes to take effect.'
				: 'Saved.';
		} catch (err) {
			if (isConflictError(err)) conflict = true;
			else saveError = errorMessage(err);
		} finally {
			saving = false;
		}
	}

	function discard(): void {
		if (loaded) contents = loaded.effectiveContents;
		saveError = null;
		notice = null;
	}

	async function reloadDiscardingEdits(): Promise<void> {
		notice = null;
		await load();
	}

	beforeNavigate(({ cancel, willUnload }) => {
		if (!dirty) return;
		// For full page unloads the browser shows its own prompt when we cancel.
		if (willUnload || !confirm('Discard unsaved changes to the site configuration?')) cancel();
	});

	onMount(() => {
		void load();
		void loadSchema();
	});
</script>

<svelte:head><title>Site configuration · Sourcegull</title></svelte:head>

<div class="page-header">
	<div>
		<h1>Site configuration</h1>
		<p class="page-description">
			{#if view === 'settings'}
				Instance-wide settings. Changes are written into the configuration document, keeping its comments; save to
				apply them.
			{:else}
				JSON with comments. Press <kbd>Ctrl</kbd>/<kbd>⌘</kbd>+<kbd>S</kbd> to save. The JSON Schema is at
				<a href="/.api/site-config-schema" target="_blank" rel="noopener">/.api/site-config-schema</a>.
			{/if}
		</p>
	</div>
	{#if view === 'json'}
		<div class="page-actions">
			<button
				class="btn"
				type="button"
				onclick={organize}
				disabled={!loaded || organizing || saving}
				title="Put the keys in section order under section headers, keeping their comments"
			>
				{organizing ? 'Organizing…' : 'Organize by section'}
			</button>
		</div>
	{/if}
</div>

<div class="tabs view-tabs" role="tablist" aria-label="Editor">
	<button
		class="tab"
		type="button"
		role="tab"
		id="tab-settings"
		aria-selected={view === 'settings'}
		aria-controls="config-panel"
		disabled={schemaFailed}
		onclick={() => showView('settings')}
	>
		Settings
	</button>
	<button
		class="tab"
		type="button"
		role="tab"
		id="tab-json"
		aria-selected={view === 'json'}
		aria-controls="config-panel"
		onclick={() => showView('json')}
	>
		Edit as JSON
	</button>
</div>

{#if loadError}
	<div class="alert alert-danger" role="alert">
		Could not load the site configuration: {loadError}
		<div><button class="btn btn-sm" type="button" onclick={load}>Retry</button></div>
	</div>
{/if}

{#if readOnly}
	<div class="alert alert-info" role="status">
		This configuration is read from a file on the server, so it can't be changed here. Edit the file instead.
	</div>
{/if}

<DeclaredConfigFiles files={declaredFiles} />

{#if conflict}
	<div class="alert alert-warning" role="alert">
		<strong>The configuration was changed since you loaded it.</strong> Someone else (or another tab)
		saved a newer version. Reload to get the latest version; your edits will be lost, so copy them
		first if you need them.
		<div class="toolbar actions">
			<button class="btn btn-sm" type="button" onclick={reloadDiscardingEdits}>Reload latest version</button>
			<button class="btn btn-sm" type="button" onclick={() => navigator.clipboard?.writeText(contents)}>
				Copy my edits
			</button>
		</div>
	</div>
{/if}

{#if saveError}
	<div class="alert alert-danger" role="alert">
		<strong>Save failed.</strong>
		{#if view === 'settings' && issues.length > 0}Problems are also shown next to the affected settings.{/if}
		<pre class="error-text">{saveError}</pre>
	</div>
{/if}

{#if notice}
	<div class="alert alert-success" role="status">{notice}</div>
{/if}

{#if loaded && loaded.validationMessages.length > 0 && !saveError}
	<div class="alert alert-warning" role="status">
		<strong>The saved configuration has {loaded.validationMessages.length} validation problem{loaded
				.validationMessages.length === 1
				? ''
				: 's'}:</strong>
		<ul>
			{#each loaded.validationMessages as message, i (i)}
				<li>{message}</li>
			{/each}
		</ul>
	</div>
{/if}

{#if loaded}
	<div
		class="layout"
		class:with-nav={sections.length > 0}
		id="config-panel"
		role="tabpanel"
		aria-labelledby={view === 'settings' ? 'tab-settings' : 'tab-json'}
	>
		{#if sections.length > 0}
			<div class="nav-select field">
				<label class="label" for="section-select">Section</label>
				<select
					id="section-select"
					class="input"
					value={selected?.id ?? ''}
					onchange={(e) => {
						const s = sections.find((x) => x.id === (e.currentTarget as HTMLSelectElement).value);
						if (s) reveal(s);
					}}
				>
					{#if !selected}<option value="">Jump to a section…</option>{/if}
					{#each sections as section (section.id)}
						<option value={section.id}>{section.title}</option>
					{/each}
				</select>
			</div>
			<nav class="sections" aria-label="Configuration sections">
				<ul>
					{#each sections as section (section.id)}
						{@const count = setCount(section, parsed)}
						{@const problems = sectionIssues(section)}
						<li>
							<button
								type="button"
								class="section"
								class:active={section.id === selected?.id}
								aria-current={section.id === selected?.id ? 'true' : undefined}
								onclick={() => reveal(section)}
							>
								<span class="section-title">
									{section.title}
									{#if count > 0}<span class="count" title="Keys set in the document">{count}</span>{/if}
									{#if problems > 0}<span class="badge badge-danger" title="Validation problems">{problems}</span>{/if}
								</span>
								<span class="section-description">{section.description}</span>
							</button>
						</li>
					{/each}
				</ul>
			</nav>
		{/if}
		<div class="main">
			{#if view === 'settings'}
				{#if !schema}
					<p class="loading" aria-live="polite"><span class="spinner"></span> Loading settings…</p>
				{:else if parsed === null}
					<div class="alert alert-warning" role="status">
						The configuration document has a syntax error, so the settings can't be shown.
						<button class="btn btn-sm" type="button" onclick={() => showView('json')}>Fix it in the JSON editor</button>
					</div>
				{:else if selected}
					<section class="card settings" aria-labelledby="settings-title">
						<div class="settings-head">
							<h2 id="settings-title">{selected.title}</h2>
							<p class="muted">{selected.description}</p>
						</div>
						{#each fields as field (field.key)}
							{#if field.key === 'auth.providers'}
								<AuthProvidersEditor
									{schema}
									doc={parsed}
									saved={savedDoc}
									{issues}
									disabled={readOnly}
									onchange={setValue}
								/>
							{:else}
								<ConfigField {field} doc={parsed} saved={savedDoc} {issues} disabled={readOnly} onchange={setValue} />
							{/if}
						{/each}
					</section>
				{/if}
			{:else}
				<div bind:this={editorHost}>
					<JsoncEditor bind:value={contents} {readOnly} label="Site configuration (JSON with comments)" onsave={save} />
				</div>
				{#if selected}
					<section class="summary card" aria-labelledby="summary-title">
						<h2 id="summary-title">{selected.title}</h2>
						<p class="muted">
							{selected.description}
							{#if !selectedInDocument}
								None of these keys is in the document, so the defaults apply. Add a key to change it.
							{/if}
						</p>
						{#if parsed === null}
							<p class="muted">The document has a syntax error; values are shown once it parses.</p>
						{/if}
						<div class="table-scroll">
							<table class="table table-sm">
								<thead><tr><th>Key</th><th>Value</th><th>Default</th></tr></thead>
								<tbody>
									{#each rows as row (row.key)}
										<tr>
											<td>
												<code>{row.key}</code>
												{#if row.description}<div class="muted small">{row.description}</div>{/if}
											</td>
											<td class="value">
												{#if row.set}
													<code title={row.value}>{row.value}</code>
													{#if row.isDefault}<span class="badge">default</span>{/if}
												{:else}
													<span class="muted">not set</span>
												{/if}
											</td>
											<td class="value">
												{#if row.default !== null}<code title={row.default}>{row.default}</code>{:else}<span
														class="muted">—</span
													>{/if}
											</td>
										</tr>
									{/each}
								</tbody>
							</table>
						</div>
					</section>
				{/if}
			{/if}
		</div>
	</div>

	{#if dirty || saving}
		<div class="save-bar" role="region" aria-label="Unsaved changes">
			<span class="save-status" aria-live="polite">
				<span class="badge badge-warning">Unsaved changes</span>
				<span class="muted small">Saved settings apply to the whole instance.</span>
			</span>
			<span class="save-actions">
				<button class="btn" type="button" onclick={discard} disabled={!dirty || saving}>Discard</button>
				<button class="btn btn-primary" type="button" onclick={save} disabled={!dirty || saving || conflict || readOnly}>
					{saving ? 'Saving…' : 'Save'}
				</button>
			</span>
		</div>
	{/if}
{:else if !loadError}
	<p class="loading" aria-live="polite"><span class="spinner"></span> Loading…</p>
{/if}

<style>
	.view-tabs {
		margin-bottom: var(--space-4);
	}
	.layout.with-nav {
		display: grid;
		grid-template-columns: minmax(12rem, 16rem) minmax(0, 1fr);
		gap: var(--space-4);
		align-items: start;
	}
	.nav-select {
		display: none;
	}
	@media (max-width: 900px) {
		.layout.with-nav {
			grid-template-columns: minmax(0, 1fr);
		}
		.sections {
			display: none;
		}
		.nav-select {
			display: flex;
			margin-bottom: 0;
		}
	}
	.sections {
		position: sticky;
		top: calc(var(--nav-height) + var(--space-4));
	}
	.sections ul {
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.section {
		display: block;
		width: 100%;
		text-align: left;
		padding: var(--space-2);
		border: 0;
		border-left: 3px solid transparent;
		border-radius: 0 var(--radius) var(--radius) 0;
		background: none;
		color: inherit;
		font: inherit;
		cursor: pointer;
	}
	.section:hover {
		background: var(--color-bg-hover);
	}
	.section.active {
		border-left-color: var(--color-primary);
		background: var(--color-primary-subtle);
	}
	.section-title {
		display: flex;
		align-items: center;
		gap: var(--space-1);
		font-weight: var(--weight-medium);
	}
	.section-description {
		display: block;
		font-size: var(--text-xs);
		color: var(--color-text-muted);
	}
	.count {
		font-size: var(--text-xs);
		font-weight: var(--weight-medium);
		padding: 0 0.4em;
		border-radius: var(--radius-full);
		background: var(--color-bg-muted);
		color: var(--color-text-muted);
		font-variant-numeric: tabular-nums;
	}
	.settings {
		padding-bottom: var(--space-2);
	}
	.settings-head {
		padding-bottom: var(--space-3);
		border-bottom: 1px solid var(--color-border);
	}
	.settings-head h2,
	.summary h2 {
		margin: 0 0 var(--space-1);
		font-size: var(--text-lg);
	}
	.settings-head p {
		margin: 0;
	}
	.summary {
		margin-top: var(--space-4);
	}
	.summary .table-scroll {
		margin: 0 calc(-1 * var(--space-6)) calc(-1 * var(--space-5));
	}
	.summary .table tbody tr:last-child > * {
		border-bottom: none;
	}
	.value code {
		display: inline-block;
		max-width: 28ch;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		vertical-align: bottom;
	}
	.actions {
		margin-top: var(--space-2);
	}
	.error-text {
		margin: var(--space-1) 0 0;
		white-space: pre-wrap;
		font-size: var(--text-sm);
	}
	.save-bar {
		position: sticky;
		bottom: 0;
		z-index: 5;
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		justify-content: space-between;
		gap: var(--space-2) var(--space-4);
		margin-top: var(--space-4);
		padding: var(--space-3) var(--space-4);
		background: var(--color-surface-raised);
		border: 1px solid var(--color-border-strong);
		border-radius: var(--radius-lg);
		box-shadow: var(--shadow-lg);
	}
	.save-status,
	.save-actions {
		display: inline-flex;
		flex-wrap: wrap;
		align-items: center;
		gap: var(--space-2);
	}
</style>
