/**
 * CodeMirror 6 setup for editing YAML bulk change specs: a small stream tokenizer for highlighting
 * (keys, scalars, block scalars, comments and `${{ … }}` templates) and the lint extension, which
 * shows the server's validation issues (pushed with `setDiagnostics`). There is no YAML parse in
 * the browser: the server validates the spec and reports positions.
 */
import { closeBrackets, closeBracketsKeymap } from '@codemirror/autocomplete';
import { defaultKeymap, history, historyKeymap, indentWithTab } from '@codemirror/commands';
import {
	HighlightStyle,
	indentOnInput,
	StreamLanguage,
	syntaxHighlighting,
	type StreamParser
} from '@codemirror/language';
import { lintGutter } from '@codemirror/lint';
import { highlightSelectionMatches, searchKeymap } from '@codemirror/search';
import { EditorState, type Extension } from '@codemirror/state';
import {
	drawSelection,
	EditorView,
	highlightActiveLine,
	highlightActiveLineGutter,
	keymap,
	lineNumbers
} from '@codemirror/view';
import { tags } from '@lezer/highlight';

interface YamlState {
	/** Indentation of the line that opened a block scalar (`|`, `>`); its content is deeper. */
	blockIndent: number | null;
	/** Indentation of the current line. */
	lineIndent: number;
	/** A mapping key may start here (start of line or after `- `). */
	keyAllowed: boolean;
	/** Column of the current line's last mapping key (a block scalar's content is deeper). */
	keyColumn: number | null;
}

const VALUE_END = /(?=\s*(?:#|$|,|\]|\}))/.source;
const KEY = /^(?:"(?:[^"\\]|\\.)*"|'(?:[^']|'')*'|[^\s#'"{[\]},:-][^#]*?|-[^\s#][^#]*?)(?=\s*:(?:\s|$))/;
const BOOL = new RegExp(`^(?:true|false|yes|no|on|off|null|~)${VALUE_END}`, 'i');
const NUMBER = new RegExp(`^[-+]?(?:0x[0-9a-f]+|\\d[\\d_]*(?:\\.\\d+)?(?:e[-+]?\\d+)?)${VALUE_END}`, 'i');

/** Exported for tests: token names are `@lezer/highlight` tag names. */
export const yamlStreamParser: StreamParser<YamlState> = {
	name: 'yaml',
	startState: () => ({ blockIndent: null, lineIndent: 0, keyAllowed: true, keyColumn: null }),
	copyState: (s) => ({ ...s }),
	token(stream, state) {
		if (stream.sol()) {
			const indent = stream.indentation();
			if (state.blockIndent !== null) {
				if (stream.match(/^\s*$/, false) || indent > state.blockIndent) {
					stream.skipToEnd();
					return 'string';
				}
				state.blockIndent = null;
			}
			state.lineIndent = indent;
			state.keyAllowed = true;
			state.keyColumn = null;
		}
		if (stream.eatSpace()) return null;
		if (stream.match(/^#.*/)) return 'comment';
		if (stream.match('${{')) {
			while (!stream.eol() && !stream.match('}}')) stream.next();
			return 'meta';
		}
		if (state.keyAllowed && stream.match(/^-(?=\s|$)/)) return 'punctuation';
		if (state.keyAllowed && stream.match(/^(?:---|\.\.\.)(?=\s|$)/)) return 'meta';
		const column = stream.column();
		if (state.keyAllowed && stream.match(KEY)) {
			state.keyAllowed = false;
			state.keyColumn = column;
			return 'propertyName';
		}
		if (stream.match(/^:(?=\s|$)/)) {
			state.keyAllowed = false;
			return 'punctuation';
		}
		state.keyAllowed = false;
		if (stream.match(/^[|>][-+]?\d?(?=\s*(?:#.*)?$)/)) {
			state.blockIndent = state.keyColumn ?? state.lineIndent;
			return 'operator';
		}
		if (stream.match(/^"(?:[^"\\]|\\.)*"?/) || stream.match(/^'(?:[^']|'')*'?/)) return 'string';
		if (stream.match(BOOL)) return 'bool';
		if (stream.match(NUMBER)) return 'number';
		if (stream.match(/^[[\]{},]/)) return 'punctuation';
		if (stream.match(/^[&*][^\s,[\]{}]+/)) return 'labelName';
		// A plain scalar, up to a template or a comment.
		while (!stream.eol()) {
			if (stream.match('${{', false) || stream.match(/^\s#/, false)) break;
			stream.next();
		}
		return 'string';
	},
	indent: () => null,
	languageData: { commentTokens: { line: '#' } }
};

const highlightStyle = HighlightStyle.define([
	{ tag: tags.propertyName, color: 'var(--syntax-property)' },
	{ tag: tags.string, color: 'var(--syntax-string)' },
	{ tag: tags.number, color: 'var(--syntax-number)' },
	{ tag: tags.bool, color: 'var(--syntax-keyword)' },
	{ tag: tags.comment, color: 'var(--syntax-comment)', fontStyle: 'italic' },
	{ tag: tags.meta, color: 'var(--syntax-function)', fontWeight: '600' },
	{ tag: tags.operator, color: 'var(--syntax-keyword)' },
	{ tag: tags.labelName, color: 'var(--syntax-type)' },
	{ tag: tags.punctuation, color: 'var(--syntax-punctuation)' }
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
	'.cm-tooltip': {
		backgroundColor: 'var(--color-surface)',
		border: '1px solid var(--color-border)',
		color: 'var(--color-text)'
	},
	'.cm-diagnostic-error': { borderLeftColor: 'var(--color-danger)' },
	'.cm-lintRange-error': {
		backgroundImage: 'none',
		textDecoration: 'underline wavy var(--color-danger)',
		textUnderlineOffset: '3px'
	},
	'.cm-panels': { backgroundColor: 'var(--color-bg-subtle)', color: 'var(--color-text)' }
});

export interface YamlEditorOptions {
	readOnly?: boolean;
	/** Called on Mod-s. */
	onSave?: () => void;
	/** Called with the new document text whenever it changes. */
	onChange?: (text: string) => void;
	/** Accessible name for the editable region. */
	label?: string;
}

/** Builds the extension set for a YAML spec editor. */
export function yamlExtensions(options: YamlEditorOptions = {}): Extension[] {
	const extensions: Extension[] = [
		lineNumbers(),
		lintGutter(),
		highlightActiveLineGutter(),
		history(),
		drawSelection(),
		indentOnInput(),
		closeBrackets(),
		highlightActiveLine(),
		highlightSelectionMatches(),
		StreamLanguage.define(yamlStreamParser),
		syntaxHighlighting(highlightStyle),
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
