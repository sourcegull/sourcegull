<script lang="ts">
	import { Compartment, EditorState } from '@codemirror/state';
	import { EditorView } from '@codemirror/view';
	import { onMount, untrack } from 'svelte';
	import {
		blameGutter,
		codeViewExtensions,
		scrollToLine,
		setBlame,
		setOccurrences,
		setSelectedLines
	} from '#lib/editor/codeview.ts';
	import { codeNavExtension, type CodeNavHandlers } from '#lib/editor/codenav.ts';
	import type { BlameHunk } from './blame.ts';
	import type { Occurrence } from './highlight.ts';
	import type { LineRange } from './url.ts';

	interface Props {
		content: string;
		occurrences: readonly Occurrence[];
		selection: LineRange | null;
		/** Blame hunks; null hides the blame gutter. */
		blame: readonly BlameHunk[] | null;
		label?: string;
		/** The number shown for the first line (for a snippet of a file). */
		firstLine?: number;
		onlineclick?: (line: number, extend: boolean) => void;
		/** Hover and go-to-definition; recreated with each new content. */
		codeNav?: CodeNavHandlers;
	}

	let { content, occurrences, selection, blame, label, firstLine = 1, onlineclick, codeNav }: Props = $props();

	let host: HTMLDivElement;
	let view = $state.raw<EditorView | undefined>();
	const blameCompartment = new Compartment();
	const navCompartment = new Compartment();

	onMount(() => {
		const v = new EditorView({
			parent: host,
			state: EditorState.create({
				doc: content,
				extensions: [
					codeViewExtensions({ label, firstLine, onLineClick: (line, extend) => onlineclick?.(line, extend) }),
					blameCompartment.of(blame ? blameGutter : []),
					navCompartment.of(codeNav ? codeNavExtension(codeNav) : [])
				]
			})
		});
		v.dispatch({ effects: [setOccurrences.of(occurrences), setSelectedLines.of(selection)] });
		if (selection) scrollToLine(v, selection.start);
		view = v;
		return () => v.destroy();
	});

	// New file content (navigation between files reuses the component).
	$effect(() => {
		const text = content;
		const occ = occurrences;
		const v = view;
		if (!v) return;
		if (text !== v.state.doc.toString()) {
			// The selection may have been set from the new URL before the content arrived (go to
			// a definition in another file): replacing the document drops its decoration.
			const sel = untrack(() => selection);
			v.dispatch({
				changes: { from: 0, to: v.state.doc.length, insert: text },
				// A fresh extension drops symbol lookups cached for the previous file.
				effects: [navCompartment.reconfigure(codeNav ? codeNavExtension(codeNav) : []), setSelectedLines.of(sel)]
			});
			if (sel && sel.start <= v.state.doc.lines) scrollToLine(v, sel.start);
		}
		v.dispatch({ effects: setOccurrences.of(occ) });
	});

	// Selected lines; scroll only when the selection is off screen (e.g. hash navigation).
	$effect(() => {
		const sel = selection;
		const v = view;
		if (!v) return;
		v.dispatch({ effects: setSelectedLines.of(sel) });
		if (sel && sel.start <= v.state.doc.lines) {
			const pos = v.state.doc.line(sel.start).from;
			if (pos < v.viewport.from || pos > v.viewport.to) scrollToLine(v, sel.start);
		}
	});

	// Blame gutter visibility and content.
	$effect(() => {
		const hunks = blame;
		const v = view;
		if (!v) return;
		v.dispatch({
			effects: [blameCompartment.reconfigure(hunks ? blameGutter : []), setBlame.of(hunks)]
		});
	});
</script>

<div class="code-view" bind:this={host}></div>

<style>
	.code-view {
		height: 100%;
		min-height: 0;
	}
	.code-view :global(.cm-editor) {
		height: 100%;
	}
</style>
