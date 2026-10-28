<script lang="ts">
	import { EditorState } from '@codemirror/state';
	import { EditorView } from '@codemirror/view';
	import { onMount } from 'svelte';
	import { jsoncExtensions } from '#lib/editor/jsonc.ts';

	interface Props {
		/** Document text; updated as the user types. */
		value: string;
		readOnly?: boolean;
		label?: string;
		onsave?: () => void;
	}

	let { value = $bindable(), readOnly = false, label = 'JSON editor', onsave }: Props = $props();

	let host: HTMLDivElement;
	let view: EditorView | undefined;

	onMount(() => {
		view = new EditorView({
			parent: host,
			state: EditorState.create({
				doc: value,
				extensions: jsoncExtensions({
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

	// Replace the document when `value` is changed from outside (e.g. after reload or discard).
	$effect(() => {
		const next = value;
		if (view && next !== view.state.doc.toString()) {
			view.dispatch({ changes: { from: 0, to: view.state.doc.length, insert: next } });
		}
	});
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
