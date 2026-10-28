<script lang="ts">
	import { untrack } from 'svelte';
	import ConfigField from './ConfigField.svelte';
	import DocText from './DocText.svelte';
	import {
		displayValue,
		getAt,
		issuesAt,
		jsonEqual,
		type ConfigIssue,
		type ConfigPath,
		type FormField
	} from '#lib/site-config-form.ts';

	interface Props {
		field: FormField;
		/** The document being edited, parsed. */
		doc: Record<string, unknown>;
		/** The document as last saved, parsed (for the "Changed" marker). */
		saved: Record<string, unknown> | null;
		issues: ConfigIssue[];
		disabled?: boolean;
		/** Sets (or, with undefined, removes) the value at `path`. */
		onchange: (path: ConfigPath, value: unknown) => void;
		/** The default inherited from an enclosing object's default, if any. */
		inheritedDefault?: unknown;
	}

	let { field, doc, saved, issues, disabled = false, onchange, inheritedDefault }: Props = $props();

	const id = $derived('cfg-' + field.path.map((p) => String(p).replace(/[^\w-]/g, '_')).join('--'));
	const value = $derived(getAt(doc, field.path));
	const isSet = $derived(value !== undefined);
	const fallback = $derived(field.default !== undefined ? field.default : inheritedDefault);
	const effective = $derived(isSet ? value : fallback);
	const changed = $derived(saved !== null && !jsonEqual(value, getAt(saved, field.path)));
	const leaf = $derived(field.kind !== 'object');
	const fieldIssues = $derived(issuesAt(issues, field.path, leaf));
	const invalid = $derived(fieldIssues.length > 0);
	const describedBy = $derived(
		[field.description ? `${id}-hint` : '', invalid ? `${id}-error` : ''].filter(Boolean).join(' ') || undefined
	);

	function set(v: unknown): void {
		onchange(field.path, v);
	}
	function reset(): void {
		onchange(field.path, undefined);
	}

	// Secrets: the stored value is never put into an input; typing replaces it.
	let editingSecret = $state(false);
	let secretDraft = $state('');
	/** Shows the value being typed (never the stored one, which is not in the input). */
	let showSecret = $state(false);
	function startSecret(): void {
		editingSecret = true;
		secretDraft = '';
	}
	const savedSecret = $derived.by(() => {
		const v = getAt(saved, field.path);
		return typeof v === 'string' && v !== '' ? v : undefined;
	});
	// A reload (after saving or discarding) ends any replacement in progress.
	$effect(() => {
		void saved;
		editingSecret = false;
		secretDraft = '';
	});
	/** The value this field last wrote; `value` lags behind it until the document is re-parsed. */
	let lastSecretSet: unknown = undefined;
	function setSecret(v: unknown): void {
		lastSecretSet = v;
		set(v);
	}
	// Discarding edits elsewhere drops the typed value too: only a change of `value` that this
	// field did not make (typing must not clear the draft while the document catches up).
	$effect(() => {
		const v = value;
		untrack(() => {
			if (editingSecret && secretDraft !== '' && v !== lastSecretSet) {
				editingSecret = false;
				secretDraft = '';
			}
		});
	});
	function cancelSecret(): void {
		editingSecret = false;
		secretDraft = '';
		setSecret(getAt(saved, field.path));
	}

	function onNumber(e: Event): void {
		const raw = (e.currentTarget as HTMLInputElement).value.trim();
		if (raw === '') return set(undefined);
		const n = Number(raw);
		if (!Number.isFinite(n) || (field.kind === 'integer' && !Number.isInteger(n))) return;
		set(n);
	}

	function onString(e: Event): void {
		const v = (e.currentTarget as HTMLInputElement).value;
		set(v === '' && !field.required ? undefined : v);
	}

	// String lists.
	let itemDraft = $state('');
	const items = $derived(Array.isArray(effective) ? (effective as unknown[]).map(String) : []);
	function addItem(): void {
		const v = itemDraft.trim();
		if (!v) return;
		set([...items, ...v.split(/[\s,]+/).filter((s) => s && !items.includes(s))]);
		itemDraft = '';
	}
	function removeItem(i: number): void {
		set(items.filter((_, j) => j !== i));
	}

	// Raw JSON fallback: a local draft, applied on blur when it parses.
	let jsonDraft = $state<string | null>(null);
	let jsonError = $state<string | null>(null);
	const jsonText = $derived(isSet ? JSON.stringify(value, null, 2) : '');
	function applyJson(): void {
		if (jsonDraft === null) return;
		const text = jsonDraft.trim();
		jsonError = null;
		if (text === '') {
			set(undefined);
			jsonDraft = null;
			return;
		}
		try {
			set(JSON.parse(text));
			jsonDraft = null;
		} catch (err) {
			jsonError = `Not valid JSON: ${(err as Error).message}`;
		}
	}

	const selectedOption = $derived(field.options?.find((o) => o.value === effective));
</script>

{#snippet header()}
	<div class="field-head">
		<span class="field-title">
			{#if field.kind === 'object'}
				<span class="label">{field.title}</span>
			{:else}
				<label class="label" for={id}>{field.title}</label>
			{/if}
			{#if field.required}<span class="badge">Required</span>{/if}
			{#if field.secret}<span class="badge" title="Stored in the site configuration; shown masked">Secret</span>{/if}
			{#if changed}<span class="badge badge-warning">Changed</span>{/if}
		</span>
		<code class="field-key">{field.key}</code>
		{#if isSet && !field.required}
			<button class="btn btn-link btn-sm reset" type="button" onclick={reset} {disabled}
				title="Remove {field.key} from the configuration so the default applies">
				{fallback !== undefined ? 'Reset to default' : 'Clear'}
			</button>
		{/if}
	</div>
	{#if field.description}
		<p class="hint" id="{id}-hint"><DocText text={field.description} /></p>
	{/if}
{/snippet}

{#snippet errors()}
	{#if invalid}
		<ul class="field-errors" id="{id}-error">
			{#each fieldIssues as issue, i (i)}
				<li class="error-text">{issue.message}</li>
			{/each}
		</ul>
	{/if}
{/snippet}

{#if field.kind === 'object' && field.children}
	<fieldset class="object" aria-describedby={describedBy}>
		<legend class="visually-hidden">{field.title}</legend>
		{@render header()}
		{@render errors()}
		<div class="children">
			{#each field.children as child (child.key)}
				<ConfigField
					field={child}
					{doc}
					{saved}
					{issues}
					{disabled}
					{onchange}
					inheritedDefault={fallback && typeof fallback === 'object'
						? (fallback as Record<string, unknown>)[child.key]
						: undefined}
				/>
			{/each}
		</div>
	</fieldset>
{:else}
	<div class="config-field" class:is-invalid={invalid}>
		{@render header()}
		{#if field.kind === 'boolean'}
			<label class="switch">
				<input
					{id}
					type="checkbox"
					role="switch"
					checked={effective === true}
					{disabled}
					aria-describedby={describedBy}
					aria-invalid={invalid || undefined}
					onchange={(e) => set((e.currentTarget as HTMLInputElement).checked)}
				/>
				<span>{effective === true ? 'On' : 'Off'}</span>
				{#if !isSet && fallback !== undefined}<span class="subtle small">(default)</span>{/if}
			</label>
		{:else if field.kind === 'enum' && field.options}
			<select
				{id}
				class="input"
				{disabled}
				aria-describedby={describedBy}
				aria-invalid={invalid || undefined}
				value={isSet ? String(value) : ''}
				onchange={(e) => {
					const v = (e.currentTarget as HTMLSelectElement).value;
					set(v === '' ? undefined : v);
				}}
			>
				{#if !field.required}
					<option value="">Default{fallback !== undefined ? ` (${displayValue(fallback)})` : ''}</option>
				{/if}
				{#each field.options as option (option.value)}
					<option value={option.value}>{option.value}</option>
				{/each}
			</select>
			{#if selectedOption?.description}<p class="hint option-hint">{selectedOption.description}</p>{/if}
		{:else if field.kind === 'string' && field.secret}
			{#if editingSecret || !isSet || value === ''}
				<div class="row">
					<input
						{id}
						class="input mono"
						type={showSecret ? 'text' : 'password'}
						autocomplete="new-password"
						spellcheck="false"
						placeholder={editingSecret && savedSecret ? 'Type a new value' : field.required ? 'Required' : 'Not set'}
						{disabled}
						aria-describedby={describedBy}
						aria-invalid={invalid || undefined}
						bind:value={secretDraft}
						oninput={() => {
							editingSecret = true;
							setSecret(secretDraft !== '' ? secretDraft : (savedSecret ?? (field.required ? '' : undefined)));
						}}
					/>
					<button
						class="btn btn-sm"
						type="button"
						aria-pressed={showSecret}
						aria-controls={id}
						onclick={() => (showSecret = !showSecret)}>{showSecret ? 'Hide' : 'Show'}</button
					>
					{#if editingSecret && savedSecret}
						<button class="btn btn-sm" type="button" onclick={cancelSecret}>Keep current</button>
					{/if}
				</div>
			{:else}
				<div class="row">
					<span class="secret-set" {id} aria-describedby={describedBy}>
						••••••••••••
						<span class="subtle small">{changed ? 'new value, not saved yet' : 'stored'}</span>
					</span>
					<button class="btn btn-sm" type="button" onclick={startSecret} {disabled}>Replace</button>
				</div>
			{/if}
		{:else if field.kind === 'string'}
			<input
				{id}
				class="input"
				type="text"
				spellcheck="false"
				placeholder={displayValue(fallback) || (field.required ? 'Required' : '')}
				minlength={field.minLength}
				{disabled}
				aria-describedby={describedBy}
				aria-invalid={invalid || undefined}
				value={typeof value === 'string' ? value : value === undefined || value === null ? '' : String(value)}
				oninput={onString}
			/>
		{:else if field.kind === 'integer' || field.kind === 'number'}
			<input
				{id}
				class="input number"
				type="number"
				inputmode={field.kind === 'integer' ? 'numeric' : 'decimal'}
				step={field.kind === 'integer' ? 1 : 'any'}
				min={field.min}
				max={field.max}
				placeholder={displayValue(fallback)}
				{disabled}
				aria-describedby={describedBy}
				aria-invalid={invalid || undefined}
				value={typeof value === 'number' ? value : ''}
				onchange={onNumber}
			/>
			{#if field.min !== undefined || field.max !== undefined}
				<p class="hint small">
					{#if field.min !== undefined && field.max !== undefined}Between {field.min} and {field.max}.{:else if field.min !== undefined}At
						least {field.min}.{:else}At most {field.max}.{/if}
				</p>
			{/if}
		{:else if field.kind === 'stringList'}
			<div class="list" aria-describedby={describedBy}>
				{#if items.length > 0}
					<ul class="tags">
						{#each items as item, i (i)}
							<li class="tag">
								<span class="mono">{item}</span>
								<button
									type="button"
									class="tag-remove"
									aria-label="Remove {item}"
									{disabled}
									onclick={() => removeItem(i)}>×</button
								>
							</li>
						{/each}
					</ul>
				{:else}
					<p class="subtle small">Empty{!isSet && fallback !== undefined ? ' (default)' : ''}.</p>
				{/if}
				<div class="row">
					<input
						{id}
						class="input input-sm"
						type="text"
						spellcheck="false"
						placeholder="Add a value and press Enter"
						{disabled}
						aria-invalid={invalid || undefined}
						bind:value={itemDraft}
						onkeydown={(e) => {
							if (e.key === 'Enter') {
								e.preventDefault();
								addItem();
							}
						}}
					/>
					<button class="btn btn-sm" type="button" onclick={addItem} disabled={disabled || !itemDraft.trim()}>Add</button>
				</div>
			</div>
		{:else}
			<textarea
				{id}
				class="input mono json"
				spellcheck="false"
				rows={Math.min(12, Math.max(3, (jsonDraft ?? jsonText).split('\n').length))}
				placeholder={fallback !== undefined ? JSON.stringify(fallback, null, 2) : 'Not set'}
				{disabled}
				aria-describedby={describedBy}
				aria-invalid={invalid || jsonError !== null || undefined}
				value={jsonDraft ?? jsonText}
				oninput={(e) => (jsonDraft = (e.currentTarget as HTMLTextAreaElement).value)}
				onblur={applyJson}
			></textarea>
			<p class="hint small">Edit this value as JSON; it is applied when you leave the field.</p>
			{#if jsonError}<p class="error-text small" role="alert">{jsonError}</p>{/if}
		{/if}
		{@render errors()}
	</div>
{/if}

<style>
	.config-field {
		display: flex;
		flex-direction: column;
		gap: var(--space-1);
		padding: var(--space-3) 0;
		border-top: 1px solid var(--color-border);
	}
	.config-field:first-child,
	.children > :global(.config-field:first-child) {
		border-top: none;
	}
	.field-head {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: var(--space-1) var(--space-2);
	}
	.field-title {
		display: inline-flex;
		flex-wrap: wrap;
		align-items: center;
		gap: var(--space-2);
	}
	.field-key {
		font-size: var(--text-xs);
		color: var(--color-text-subtle);
		overflow-wrap: anywhere;
	}
	.reset {
		margin-left: auto;
		padding: 0;
		min-height: 0;
	}
	.hint {
		margin: 0;
		max-width: 48rem;
	}
	.option-hint {
		font-size: var(--text-xs);
	}
	.input {
		max-width: 32rem;
	}
	.input.number {
		max-width: 12rem;
	}
	.json {
		max-width: 48rem;
		font-size: var(--text-sm);
	}
	.row {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: var(--space-2);
	}
	.row .input {
		flex: 1 1 14rem;
	}
	.switch {
		display: inline-flex;
		align-items: center;
		gap: var(--space-2);
		width: fit-content;
		cursor: pointer;
	}
	.switch input {
		width: 1rem;
		height: 1rem;
	}
	.secret-set {
		display: inline-flex;
		align-items: center;
		gap: var(--space-2);
		min-height: var(--control-height-sm);
		padding: 0 var(--space-2);
		border: 1px dashed var(--color-border-strong);
		border-radius: var(--radius);
		font-family: var(--font-mono);
	}
	.list {
		display: flex;
		flex-direction: column;
		gap: var(--space-2);
		max-width: 32rem;
	}
	.tags {
		display: flex;
		flex-wrap: wrap;
		gap: var(--space-1);
		list-style: none;
		margin: 0;
		padding: 0;
	}
	.tag {
		display: inline-flex;
		align-items: center;
		gap: var(--space-1);
		padding: 0.1rem 0.25rem 0.1rem 0.5rem;
		border: 1px solid var(--color-border);
		border-radius: var(--radius-full);
		background: var(--color-bg-subtle);
		font-size: var(--text-sm);
		max-width: 100%;
		overflow-wrap: anywhere;
	}
	.tag-remove {
		border: 0;
		background: none;
		color: var(--color-text-muted);
		cursor: pointer;
		font-size: var(--text-md);
		line-height: 1;
		padding: 0 0.25rem;
		border-radius: var(--radius-full);
	}
	.tag-remove:hover {
		background: var(--color-bg-hover);
		color: var(--color-text);
	}
	.field-errors {
		margin: 0;
		padding-left: var(--space-4);
		font-size: var(--text-sm);
	}
	.object {
		margin: 0;
		padding: var(--space-3) 0;
		border: 0;
		border-top: 1px solid var(--color-border);
		border-radius: 0;
	}
	.children {
		margin-top: var(--space-2);
		padding-left: var(--space-4);
		border-left: 2px solid var(--color-border);
	}
	.is-invalid .field-title .label {
		color: var(--color-danger);
	}
</style>
