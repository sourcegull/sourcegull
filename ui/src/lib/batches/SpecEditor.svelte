<!--
	The YAML editor of a bulk change spec (CodeMirror), showing the server's validation issues inline
	(underlined, with a gutter marker and a tooltip). `reveal` moves the cursor to an issue.
-->
<script lang="ts">
	import { setDiagnostics, type Diagnostic } from '@codemirror/lint';
	import { EditorSelection, EditorState } from '@codemirror/state';
	import { EditorView } from '@codemirror/view';
	import { onMount } from 'svelte';
	import { yamlExtensions } from '#lib/editor/yaml.ts';
	import { formatIssue, issueRange, type SpecIssue } from './spec.ts';

	interface Props {
		/** Document text; updated as the user types. */
		value: string;
		/** Issues of the last validation, marked in the text. */
		issues?: SpecIssue[];
		readOnly?: boolean;
		label?: string;
		onsave?: () => void;
	}

	let { value = $bindable(), issues = [], readOnly = false, label = 'Bulk change spec', onsave }: Props = $props();

	let host: HTMLDivElement;
	let view = $state<EditorView | undefined>();

	onMount(() => {
		view = new EditorView({
			parent: host,
			state: EditorState.create({
				doc: value,
				extensions: yamlExtensions({
					readOnly,
					label,
					onSave: () => onsave?.(),
					onChange: (text) => {
						value = text;
					}
				})
			})
		});
		return () => view?.destroy();
	});

	// Replace the document when `value` is changed from outside (an example inserted, a reload).
	$effect(() => {
		const next = value;
		if (view && next !== view.state.doc.toString()) {
			view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: next } });
		}
	});

	$effect(() => {
		if (!view) return;
		const text = view.state.doc.toString();
		const diagnostics: Diagnostic[] = issues.map((issue) => {
			const { from, to } = issueRange(text, issue);
			return { from, to, severity: 'error', message: formatIssue(issue), source: 'bulk change spec' };
		});
		view.dispatch(setDiagnostics(view.state, diagnostics));
	});

	/** Moves the cursor to an issue's position and focuses the editor. */
	export function reveal(issue: SpecIssue): void {
		if (!view) return;
		const { from } = issueRange(view.state.doc.toString(), issue);
		view.dispatch({ selection: EditorSelection.cursor(from), scrollIntoView: true });
		view.focus();
	}
</script>

<div class="editor" bind:this={host}></div>

<style>
	.editor {
		border: 1px solid var(--color-border-strong);
		border-radius: var(--radius);
		overflow: hidden;
		height: min(70vh, 720px);
		min-height: 320px;
	}
	.editor:focus-within {
		border-color: var(--color-focus);
		box-shadow: 0 0 0 3px color-mix(in srgb, var(--color-focus) 30%, transparent);
	}
</style>
