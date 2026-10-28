<script lang="ts" module>
	/** Suggestions for a field's value (repositories, contexts…); empty for none. */
	export type ValueSuggester = (field: string, prefix: string) => Promise<string[]>;
</script>

<script lang="ts">
	import {
		acceptCompletion,
		autocompletion,
		completionKeymap,
		completionStatus,
		startCompletion,
		type Completion,
		type CompletionContext,
		type CompletionResult
	} from '@codemirror/autocomplete';
	import { defaultKeymap, history, historyKeymap } from '@codemirror/commands';
	import { linter, type Diagnostic } from '@codemirror/lint';
	import { Compartment, EditorState, Prec, type Extension } from '@codemirror/state';
	import {
		Decoration,
		EditorView,
		keymap,
		placeholder as placeholderExt,
		ViewPlugin,
		type DecorationSet,
		type ViewUpdate
	} from '@codemirror/view';
	import { onMount } from 'svelte';
	import {
		FALLBACK_FIELDS,
		FIELD_VALUES,
		loadQueryParser,
		wordAt,
		type PatternType,
		type QueryDiagnostic,
		type QueryParser
	} from './query.ts';
	import {
		matchRecentSearches,
		recallIndex,
		relativeTime,
		resultCountLabel,
		splitContext,
		stepIndex,
		type RecentSearch
	} from './recent-searches.ts';

	interface Props {
		value: string;
		patternType: PatternType;
		placeholder?: string;
		autofocus?: boolean;
		label?: string;
		onsubmit?: () => void;
		suggest?: ValueSuggester;
		ondiagnostics?: (d: QueryDiagnostic[]) => void;
		/** Recent searches, newest first: shown below the box while it has focus. */
		recent?: RecentSearch[];
		onselectrecent?: (entry: RecentSearch) => void;
		onremoverecent?: (query: string) => void;
		onclearrecent?: () => void;
	}

	let {
		value = $bindable(),
		patternType,
		placeholder = '',
		autofocus = false,
		label = 'Search query',
		onsubmit,
		suggest,
		ondiagnostics,
		recent = [],
		onselectrecent,
		onremoverecent,
		onclearrecent
	}: Props = $props();

	// The "Recent searches" dropdown: all of them
	// for an empty box, the ones containing the typed text otherwise.
	const listId = `recent-searches-${Math.random().toString(36).slice(2, 8)}`;
	let focused = $state(false);
	let dismissed = $state(false);
	let completing = $state(false);
	let active = $state(-1);
	/** Position of Ctrl/⌘+↑ history recall in `recent` (-1: not recalling). */
	let recalled = -1;
	let recalling = false;
	const matches = $derived(onselectrecent ? matchRecentSearches(recent, value) : []);
	const open = $derived(focused && !dismissed && !completing && matches.length > 0);

	function selectRecent(entry: RecentSearch): void {
		dismissed = true;
		active = -1;
		onselectrecent?.(entry);
	}

	function setDoc(editor: EditorView, text: string): void {
		recalling = true;
		editor.dispatch({
			changes: { from: 0, to: editor.state.doc.length, insert: text },
			selection: { anchor: text.length }
		});
		recalling = false;
	}

	/** Keys for the dropdown and history recall; they run before completion and editing keys. */
	const recentKeymap = keymap.of([
		{
			key: 'ArrowDown',
			run: () => {
				if (open) {
					active = stepIndex(active, matches.length, 1);
					return true;
				}
				if (dismissed && !completing && matches.length > 0) {
					dismissed = false;
					active = 0;
					return true;
				}
				return false;
			}
		},
		{
			key: 'ArrowUp',
			run: () => {
				if (!open) return false;
				active = stepIndex(active, matches.length, -1);
				return true;
			}
		},
		{
			key: 'Enter',
			run: () => {
				const entry = open ? matches[active] : undefined;
				if (!entry) return false;
				selectRecent(entry);
				return true;
			}
		},
		{
			key: 'Escape',
			run: () => {
				if (!open) return false;
				dismissed = true;
				active = -1;
				return true;
			}
		},
		{
			key: 'Shift-Delete',
			run: () => {
				const entry = open ? matches[active] : undefined;
				if (!entry) return false;
				onremoverecent?.(entry.query);
				active = Math.min(active, matches.length - 2);
				return true;
			}
		},
		...(['Mod-ArrowUp', 'Mod-ArrowDown'] as const).map((key) => ({
			key,
			run: (editor: EditorView) => {
				if (!onselectrecent || recent.length === 0) return false;
				const next = recallIndex(recalled, recent.length, key === 'Mod-ArrowUp');
				if (next === null) return true;
				recalled = next;
				setDoc(editor, next < 0 ? '' : (recent[next]?.query ?? ''));
				dismissed = true;
				return true;
			}
		}))
	]);

	let host: HTMLDivElement;
	let view: EditorView | undefined;
	let parser: QueryParser | null = null;
	const language = new Compartment();
	let fields: string[][] = FALLBACK_FIELDS.map((f) => [f]);

	const marks = new Map<string, Decoration>();
	const mark = (kind: string) => {
		let m = marks.get(kind);
		if (!m) {
			m = Decoration.mark({ class: `cm-q-${kind}` });
			marks.set(kind, m);
		}
		return m;
	};

	/** Highlighting and diagnostics from the WASM parser for the current pattern type. */
	function parserExtensions(p: QueryParser, pt: PatternType): Extension {
		const highlight = ViewPlugin.fromClass(
			class {
				decorations: DecorationSet;
				constructor(v: EditorView) {
					this.decorations = this.build(v);
				}
				update(u: ViewUpdate) {
					if (u.docChanged) this.decorations = this.build(u.view);
				}
				build(v: EditorView): DecorationSet {
					const doc = v.state.doc.toString();
					const len = doc.length;
					const ranges = p
						.analyze(doc, pt)
						.tokens.filter((t) => t.end > t.start && t.end <= len)
						.map((t) => mark(t.kind).range(t.start, t.end));
					return Decoration.set(ranges, true);
				}
			},
			{ decorations: (v) => v.decorations }
		);
		const lint = linter(
			(v): Diagnostic[] => {
				const doc = v.state.doc.toString();
				const d = p.analyze(doc, pt).diagnostics;
				ondiagnostics?.(d);
				return d.map((x) => ({
					from: Math.min(x.start, doc.length),
					to: Math.min(Math.max(x.end, x.start), doc.length),
					severity: 'error',
					message: x.message
				}));
			},
			{ delay: 150 }
		);
		return [highlight, lint];
	}

	function fieldOptions(prefix: string): Completion[] {
		const p = prefix.toLowerCase();
		const out: Completion[] = [];
		for (const names of fields) {
			const canonical = names[0] ?? '';
			const hit = names.find((n) => n.startsWith(p));
			if (!hit) continue;
			out.push({
				label: `${hit}:`,
				detail: hit === canonical ? undefined : `${canonical}:`,
				type: 'keyword',
				apply: (v, _c, from, to) => {
					v.dispatch({
						changes: { from, to, insert: `${hit}:` },
						selection: { anchor: from + hit.length + 1 }
					});
					startCompletion(v);
				}
			});
		}
		return out;
	}

	function canonicalField(name: string): string {
		return fields.find((names) => names.includes(name))?.[0] ?? name;
	}

	async function complete(ctx: CompletionContext): Promise<CompletionResult | null> {
		const text = ctx.state.doc.toString();
		const w = wordAt(text, ctx.pos);
		if (w.kind === 'word') {
			if (!ctx.explicit && w.prefix.length === 0) return null;
			const options = fieldOptions(w.prefix);
			return options.length ? { from: w.from, options, validFor: /^[A-Za-z]*$/ } : null;
		}
		const field = canonicalField(w.field);
		const fixed = FIELD_VALUES[field] ?? [];
		let values = fixed.filter((v) => v.toLowerCase().startsWith(w.prefix.toLowerCase()));
		if (fixed.length === 0 && suggest) {
			try {
				values = await suggest(field, w.prefix);
			} catch {
				values = [];
			}
			if (ctx.aborted) return null;
		}
		if (values.length === 0) return null;
		return {
			from: w.from,
			options: values.map((v) => ({ label: v, type: 'variable', apply: `${v} ` })),
			filter: fixed.length > 0
		};
	}

	/** Enter submits; newlines (from pastes) become spaces. */
	const singleLine = EditorState.transactionFilter.of((tr) => {
		if (!tr.docChanged || !/[\r\n]/.test(tr.newDoc.toString())) return tr;
		const changes: { from: number; to: number; insert: string }[] = [];
		tr.changes.iterChanges((fromA, toA, _fromB, _toB, inserted) => {
			changes.push({ from: fromA, to: toA, insert: inserted.toString().replace(/\r?\n|\r/g, ' ') });
		});
		return [{ changes, selection: tr.selection, scrollIntoView: tr.scrollIntoView }];
	});

	onMount(() => {
		view = new EditorView({
			parent: host,
			state: EditorState.create({
				doc: value,
				extensions: [
					singleLine,
					history(),
					Prec.highest(recentKeymap),
					EditorView.domEventHandlers({
						focus: () => {
							focused = true;
							dismissed = false;
						},
						mousedown: () => {
							dismissed = false;
						},
						blur: () => {
							// Removing the focused editor during a render fires blur synchronously,
							// where state must not change; apply it once the render is done.
							queueMicrotask(() => {
								focused = false;
								active = -1;
							});
						}
					}),
					Prec.high(
						keymap.of([
							{
								key: 'Enter',
								run: (editor) => {
									if (acceptCompletion(editor)) return true;
									onsubmit?.();
									return true;
								}
							}
						])
					),
					keymap.of([...completionKeymap, ...defaultKeymap, ...historyKeymap]),
					autocompletion({ override: [complete], icons: false, activateOnTyping: true }),
					placeholderExt(placeholder),
					language.of([]),
					EditorView.contentAttributes.of({
						'aria-label': label,
						spellcheck: 'false',
						autocapitalize: 'off',
						autocorrect: 'off'
					}),
					EditorView.updateListener.of((u) => {
						completing = completionStatus(u.state) !== null;
						if (u.docChanged) {
							value = u.state.doc.toString();
							active = -1;
							if (!recalling) {
								recalled = -1;
								dismissed = false;
							}
						}
					})
				]
			})
		});
		if (autofocus) {
			view.focus();
			view.dispatch({ selection: { anchor: view.state.doc.length } });
			// Recent searches open on a click, ↓ or typing, not over the page as it loads.
			dismissed = true;
		}
		void loadQueryParser().then((p) => {
			parser = p;
			if (!p || !view) return;
			fields = p.fields();
			view.dispatch({ effects: language.reconfigure(parserExtensions(p, patternType)) });
		});
		return () => view?.destroy();
	});

	// Screen readers follow the highlighted row.
	$effect(() => {
		const id = open && active >= 0 ? `${listId}-${active}` : null;
		if (!view) return;
		if (id) view.contentDOM.setAttribute('aria-activedescendant', id);
		else view.contentDOM.removeAttribute('aria-activedescendant');
	});

	// Re-highlight when the pattern type changes.
	$effect(() => {
		const pt = patternType;
		if (view && parser) view.dispatch({ effects: language.reconfigure(parserExtensions(parser, pt)) });
	});

	// Replace the document when `value` changes from outside (navigation, filter clicks).
	$effect(() => {
		const next = value;
		if (view && next !== view.state.doc.toString()) {
			view.dispatch({
				changes: { from: 0, to: view.state.doc.length, insert: next },
				selection: { anchor: next.length }
			});
		}
	});

	/** Focuses the input with the cursor at the end. */
	export function focus(): void {
		if (!view) return;
		view.focus();
		view.dispatch({ selection: { anchor: view.state.doc.length } });
	}
</script>

<div class="query-input mono">
	<div bind:this={host}></div>
	{#if open}
		<!-- Mouse down would blur the editor and close the list before the click lands. -->
		<!-- svelte-ignore a11y_no_static_element_interactions -->
		<div class="recent" onmousedown={(e) => e.preventDefault()}>
			<div class="recent-header">
				<span>Recent searches</span>
				{#if onclearrecent}
					<button type="button" class="btn-link recent-clear" onclick={() => onclearrecent?.()}>Clear all</button>
				{/if}
			</div>
			<ul id={listId} role="listbox" aria-label="Recent searches">
				{#each matches as entry, i (entry.query)}
					{@const parts = splitContext(entry.query)}
					<!-- Keyboard selection happens in the editor (↑/↓, Enter). -->
					<!-- svelte-ignore a11y_click_events_have_key_events -->
					<li
						id={`${listId}-${i}`}
						role="option"
						aria-selected={i === active}
						class:active={i === active}
						onclick={() => selectRecent(entry)}
						onmousemove={() => (active = i)}
					>
						<span class="recent-query"
							>{#if parts.context}<span class="recent-context">{parts.context}</span>{/if}{parts.rest}</span
						>
						<span class="recent-meta">{resultCountLabel(entry)} · {relativeTime(entry.timestamp)}</span>
						{#if onremoverecent}
							<button
								type="button"
								class="recent-remove"
								aria-label={`Remove ${entry.query} from recent searches`}
								title="Remove from recent searches"
								onclick={(e) => {
									e.stopPropagation();
									onremoverecent?.(entry.query);
								}}>×</button
							>
						{/if}
					</li>
				{/each}
			</ul>
			<div class="recent-hint">
				<kbd>↑</kbd><kbd>↓</kbd> navigate · <kbd>Enter</kbd> search · <kbd>Shift</kbd>+<kbd>Del</kbd> remove ·
				<kbd>Esc</kbd> close · <kbd>Ctrl</kbd>/<kbd>⌘</kbd>+<kbd>↑</kbd> previous search
			</div>
		</div>
	{/if}
</div>

<style>
	.query-input {
		position: relative;
		flex: 1;
		min-width: 0;
		border: 1px solid var(--color-border-strong);
		border-radius: var(--radius);
		background: var(--color-surface);
	}
	.query-input:focus-within {
		border-color: var(--color-focus);
		box-shadow: 0 0 0 3px color-mix(in srgb, var(--color-focus) 30%, transparent);
	}
	.query-input :global(.cm-editor) {
		background: transparent;
	}
	.query-input :global(.cm-editor.cm-focused) {
		outline: none;
	}
	.query-input :global(.cm-content) {
		padding: 0.55rem 0.7rem;
		font-family: var(--font-mono);
		font-size: 0.95rem;
		caret-color: var(--color-text);
	}
	.query-input :global(.cm-line) {
		padding: 0;
	}
	.query-input :global(.cm-scroller) {
		overflow-x: auto;
		scrollbar-width: none;
	}
	.query-input :global(.cm-placeholder) {
		color: var(--color-text-muted);
	}
	.query-input :global(.cm-q-field) {
		color: var(--syntax-property);
	}
	.query-input :global(.cm-q-negation),
	.query-input :global(.cm-q-keyword) {
		color: var(--syntax-keyword);
		font-weight: 600;
	}
	.query-input :global(.cm-q-predicate) {
		color: var(--syntax-function);
	}
	.query-input :global(.cm-q-regexp) {
		color: var(--syntax-number);
	}
	.query-input :global(.cm-q-quoted) {
		color: var(--syntax-string);
	}
	.query-input :global(.cm-q-paren) {
		color: var(--syntax-punctuation);
		font-weight: 600;
	}
	.query-input :global(.cm-tooltip-autocomplete) {
		font-family: var(--font-mono);
		background: var(--color-surface);
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
	}
	.query-input :global(.cm-tooltip-autocomplete > ul > li[aria-selected]) {
		background: var(--color-primary);
		color: var(--color-primary-text);
	}
	.query-input :global(.cm-tooltip-lint) {
		font-family: var(--font-sans);
		background: var(--color-surface);
		border: 1px solid var(--color-border);
	}
	.recent {
		position: absolute;
		top: calc(100% + 4px);
		left: -1px;
		/* At least as wide as the input; grows to fit long queries (up to a cap). */
		min-width: calc(100% + 2px);
		width: max-content;
		max-width: min(56rem, calc(100vw - 2rem));
		z-index: 30;
		background: var(--color-surface);
		border: 1px solid var(--color-border);
		border-radius: var(--radius);
		box-shadow: 0 6px 18px rgb(0 0 0 / 0.12);
		font-family: var(--font-sans);
		font-size: 0.85rem;
		overflow: hidden;
	}
	.recent-header {
		display: flex;
		justify-content: space-between;
		align-items: center;
		padding: 0.35rem 0.7rem;
		color: var(--color-text-muted);
		font-size: 0.75rem;
		font-weight: 600;
		text-transform: uppercase;
		letter-spacing: 0.03em;
	}
	.recent-clear {
		font-size: 0.75rem;
		text-transform: none;
		letter-spacing: 0;
	}
	.recent ul {
		list-style: none;
		margin: 0;
		padding: 0;
		max-height: 22rem;
		overflow-y: auto;
	}
	.recent li {
		display: flex;
		align-items: center;
		gap: var(--space-2);
		padding: 0.35rem 0.7rem;
		cursor: pointer;
	}
	.recent li.active {
		background: color-mix(in srgb, var(--color-primary) 14%, transparent);
	}
	.recent-query {
		flex: 1;
		min-width: 0;
		overflow: hidden;
		text-overflow: ellipsis;
		white-space: nowrap;
		font-family: var(--font-mono);
		color: var(--color-text);
	}
	.recent-context {
		color: var(--color-text-muted);
	}
	.recent-meta {
		flex: none;
		color: var(--color-text-muted);
		font-size: 0.8rem;
	}
	.recent-remove {
		flex: none;
		border: 0;
		background: none;
		color: var(--color-text-muted);
		font-size: 1rem;
		line-height: 1;
		padding: 0 0.2rem;
		cursor: pointer;
		visibility: hidden;
	}
	.recent li:hover .recent-remove,
	.recent li.active .recent-remove,
	.recent-remove:focus-visible {
		visibility: visible;
	}
	.recent-remove:hover {
		color: var(--color-danger);
	}
	.recent-hint {
		padding: 0.35rem 0.7rem;
		border-top: 1px solid var(--color-border);
		color: var(--color-text-muted);
		font-size: 0.75rem;
	}
	.recent-hint kbd {
		font-size: 0.7rem;
	}
	@media (max-width: 40rem) {
		.recent-meta,
		.recent-hint {
			display: none;
		}
	}
</style>
