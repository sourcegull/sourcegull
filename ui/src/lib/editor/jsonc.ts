/**
 * CodeMirror 6 setup for editing JSONC documents (JSON with comments), such as the site configuration.
 *
 * We use the JSON language for structure and highlighting but deliberately do not install the JSON
 * parse linter, because comments are valid in our config and the server reports real problems as
 * validation messages. Comments are highlighted with a small match decorator instead.
 */
import { autocompletion, closeBrackets, closeBracketsKeymap } from '@codemirror/autocomplete';
import { defaultKeymap, history, historyKeymap, indentWithTab } from '@codemirror/commands';
import { json } from '@codemirror/lang-json';
import {
	bracketMatching,
	foldGutter,
	foldKeymap,
	HighlightStyle,
	indentOnInput,
	syntaxHighlighting
} from '@codemirror/language';
import { highlightSelectionMatches, searchKeymap } from '@codemirror/search';
import { EditorState, type Extension } from '@codemirror/state';
import {
	Decoration,
	drawSelection,
	EditorView,
	highlightActiveLine,
	highlightActiveLineGutter,
	keymap,
	lineNumbers,
	MatchDecorator,
	ViewPlugin,
	type DecorationSet,
	type ViewUpdate
} from '@codemirror/view';
import { tags } from '@lezer/highlight';

const commentMark = Decoration.mark({ class: 'cm-jsonc-comment' });

/**
 * Matches strings (so `//` inside `"https://…"` is skipped) and line/single-line block comments;
 * only comments are decorated.
 */
const commentDecorator = new MatchDecorator({
	regexp: /"(?:[^"\\]|\\.)*"?|\/\/.*|\/\*.*?(?:\*\/|$)/g,
	decoration: (match) => (match[0].startsWith('"') ? null : commentMark)
});

const commentHighlighter = ViewPlugin.fromClass(
	class {
		decorations: DecorationSet;
		constructor(view: EditorView) {
			this.decorations = commentDecorator.createDeco(view);
		}
		update(update: ViewUpdate) {
			this.decorations = commentDecorator.updateDeco(update, this.decorations);
		}
	},
	{ decorations: (v) => v.decorations }
);

/** Syntax colours come from CSS custom properties so the editor follows the light/dark theme. */
const highlightStyle = HighlightStyle.define([
	{ tag: tags.string, color: 'var(--syntax-string)' },
	{ tag: tags.number, color: 'var(--syntax-number)' },
	{ tag: [tags.bool, tags.null], color: 'var(--syntax-keyword)' },
	{ tag: tags.propertyName, color: 'var(--syntax-property)' },
	{ tag: [tags.punctuation, tags.separator, tags.brace, tags.squareBracket], color: 'var(--syntax-punctuation)' },
	{ tag: tags.invalid, color: 'inherit' }
]);

const theme = EditorView.theme({
	'&': {
		color: 'var(--color-text)',
		backgroundColor: 'var(--color-bg)',
		fontSize: '13px',
		height: '100%'
	},
	'&.cm-focused': { outline: 'none' },
	'.cm-scroller': { fontFamily: 'var(--font-mono)', lineHeight: '1.55' },
	'.cm-content': { caretColor: 'var(--color-text)' },
	'.cm-cursor, .cm-dropCursor': { borderLeftColor: 'var(--color-text)' },
	'&.cm-focused > .cm-scroller > .cm-selectionLayer .cm-selectionBackground, .cm-selectionBackground':
		{ backgroundColor: 'var(--editor-selection)' },
	'.cm-gutters': {
		backgroundColor: 'var(--editor-gutter)',
		color: 'var(--color-text-muted)',
		borderRight: '1px solid var(--color-border)'
	},
	'.cm-activeLine': { backgroundColor: 'color-mix(in srgb, var(--color-bg-muted) 55%, transparent)' },
	'.cm-activeLineGutter': { backgroundColor: 'var(--color-bg-muted)' },
	'.cm-matchingBracket': { backgroundColor: 'var(--color-bg-muted)', outline: '1px solid var(--color-border-strong)' },
	'.cm-jsonc-comment, .cm-jsonc-comment *': { color: 'var(--syntax-comment)', fontStyle: 'italic' },
	'.cm-tooltip': {
		backgroundColor: 'var(--color-surface)',
		border: '1px solid var(--color-border)',
		color: 'var(--color-text)'
	},
	'.cm-panels': { backgroundColor: 'var(--color-bg-subtle)', color: 'var(--color-text)' }
});

export interface JsoncEditorOptions {
	readOnly?: boolean;
	/** Called on Mod-s. */
	onSave?: () => void;
	/** Called with the new document text whenever it changes. */
	onChange?: (text: string) => void;
	/** Accessible name for the editable region. */
	label?: string;
}

/** Builds the extension set for a JSONC editor. */
export function jsoncExtensions(options: JsoncEditorOptions = {}): Extension[] {
	const extensions: Extension[] = [
		lineNumbers(),
		highlightActiveLineGutter(),
		foldGutter(),
		history(),
		drawSelection(),
		indentOnInput(),
		bracketMatching(),
		closeBrackets(),
		autocompletion(),
		highlightActiveLine(),
		highlightSelectionMatches(),
		json(),
		syntaxHighlighting(highlightStyle),
		commentHighlighter,
		theme,
		EditorState.tabSize.of(2),
		EditorState.readOnly.of(options.readOnly === true),
		EditorView.editable.of(options.readOnly !== true),
		keymap.of([
			{
				key: 'Mod-s',
				preventDefault: true,
				run: () => {
					options.onSave?.();
					return true;
				}
			},
			...closeBracketsKeymap,
			...defaultKeymap,
			...searchKeymap,
			...historyKeymap,
			...foldKeymap,
			indentWithTab
		])
	];
	if (options.label) {
		extensions.push(EditorView.contentAttributes.of({ 'aria-label': options.label }));
	}
	if (options.onChange) {
		const onChange = options.onChange;
		extensions.push(
			EditorView.updateListener.of((update) => {
				if (update.docChanged) onChange(update.state.doc.toString());
			})
		);
	}
	return extensions;
}
