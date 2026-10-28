/**
 * CodeMirror 6 setup for the read-only file view: line numbers with click/shift-click line
 * selection, server-side highlighting applied as mark decorations (no client-side parsing), and a
 * blame gutter fed incrementally from the streaming blame endpoint.
 */
import { highlightSelectionMatches, search, searchKeymap } from '@codemirror/search';
import {
	EditorState,
	RangeSetBuilder,
	StateEffect,
	StateField,
	type Extension
} from '@codemirror/state';
import {
	Decoration,
	EditorView,
	gutter,
	GutterMarker,
	keymap,
	lineNumbers,
	type DecorationSet
} from '@codemirror/view';
import type { BlameHunk } from '#lib/repo/blame.ts';
import { relativeDate } from '#lib/repo/blame.ts';
import { kindClass, type Occurrence } from '#lib/repo/highlight.ts';
import type { LineRange } from '#lib/repo/url.ts';

/** Replaces the highlighting occurrences. */
export const setOccurrences = StateEffect.define<readonly Occurrence[]>();
/** Sets (or clears) the selected line range. */
export const setSelectedLines = StateEffect.define<LineRange | null>();
/** Replaces the blame hunks (null hides the gutter content). */
export const setBlame = StateEffect.define<readonly BlameHunk[] | null>();

const markCache = new Map<number, Decoration>();
function markFor(kind: number): Decoration | null {
	let m = markCache.get(kind);
	if (!m) {
		const cls = kindClass(kind);
		if (!cls) return null;
		m = Decoration.mark({ class: cls });
		markCache.set(kind, m);
	}
	return m;
}

/** Builds mark decorations from occurrences (sorted by line, then column). */
export function occurrenceDecorations(state: EditorState, occ: readonly Occurrence[]): DecorationSet {
	const builder = new RangeSetBuilder<Decoration>();
	const doc = state.doc;
	let lastFrom = -1;
	for (const o of occ) {
		if (o.line + 1 > doc.lines) break;
		const line = doc.line(o.line + 1);
		const from = line.from + o.startCol;
		const to = Math.min(line.from + o.endCol, line.to);
		const mark = markFor(o.kind);
		if (!mark || from >= to || from < lastFrom) continue;
		builder.add(from, to, mark);
		lastFrom = from;
	}
	return builder.finish();
}

const occurrenceField = StateField.define<DecorationSet>({
	create: () => Decoration.none,
	update(deco, tr) {
		for (const e of tr.effects) if (e.is(setOccurrences)) return occurrenceDecorations(tr.state, e.value);
		return deco.map(tr.changes);
	},
	provide: (f) => EditorView.decorations.from(f)
});

const selectedLine = Decoration.line({ class: 'cm-selected-line' });

const selectionField = StateField.define<DecorationSet>({
	create: () => Decoration.none,
	update(deco, tr) {
		for (const e of tr.effects) {
			if (!e.is(setSelectedLines)) continue;
			if (!e.value) return Decoration.none;
			const builder = new RangeSetBuilder<Decoration>();
			const end = Math.min(e.value.end, tr.state.doc.lines);
			for (let n = e.value.start; n <= end; n++) {
				const l = tr.state.doc.line(n);
				builder.add(l.from, l.from, selectedLine);
			}
			return builder.finish();
		}
		return deco.map(tr.changes);
	},
	provide: (f) => EditorView.decorations.from(f)
});

/** Blame hunks by their first line. */
const blameField = StateField.define<Map<number, BlameHunk> | null>({
	create: () => null,
	update(value, tr) {
		for (const e of tr.effects) {
			if (!e.is(setBlame)) continue;
			if (!e.value) return null;
			const m = new Map<number, BlameHunk>();
			for (const h of e.value) m.set(h.startLine, h);
			return m;
		}
		return value;
	}
});

class BlameMarker extends GutterMarker {
	constructor(readonly hunk: BlameHunk) {
		super();
	}
	override eq(other: GutterMarker): boolean {
		return other instanceof BlameMarker && other.hunk === this.hunk;
	}
	override toDOM(): Node {
		const a = document.createElement('a');
		a.className = 'cm-blame-entry';
		a.href = this.hunk.commit.url;
		a.title = `${this.hunk.commitID.slice(0, 7)} ${this.hunk.message}\n${this.hunk.author.Name} <${this.hunk.author.Email}>\n${this.hunk.author.Date}`;
		const date = document.createElement('span');
		date.className = 'cm-blame-date';
		date.textContent = relativeDate(this.hunk.author.Date);
		const who = document.createElement('span');
		who.className = 'cm-blame-author';
		who.textContent = this.hunk.author.Name;
		const msg = document.createElement('span');
		msg.className = 'cm-blame-message';
		msg.textContent = this.hunk.message;
		a.append(date, who, msg);
		return a;
	}
}

class BlameSpacer extends GutterMarker {
	override toDOM(): Node {
		const span = document.createElement('span');
		span.className = 'cm-blame-entry cm-blame-spacer';
		return span;
	}
}

/** The blame gutter; put it in a `Compartment` to toggle it. */
export const blameGutter: Extension = gutter({
	class: 'cm-blame-gutter',
	lineMarker(view, line) {
		const hunks = view.state.field(blameField);
		if (!hunks) return null;
		const h = hunks.get(view.state.doc.lineAt(line.from).number);
		return h ? new BlameMarker(h) : null;
	},
	lineMarkerChange: (update) => update.transactions.some((tr) => tr.effects.some((e) => e.is(setBlame))),
	initialSpacer: () => new BlameSpacer()
});

const theme = EditorView.theme({
	'&': {
		color: 'var(--color-text)',
		backgroundColor: 'var(--color-bg)',
		fontSize: '13px',
		height: '100%'
	},
	'&.cm-focused': { outline: 'none' },
	'.cm-scroller': { fontFamily: 'var(--font-mono)', lineHeight: '1.55' },
	'.cm-content': { caretColor: 'transparent' },
	'.cm-gutters': {
		backgroundColor: 'var(--editor-gutter)',
		color: 'var(--color-text-muted)',
		borderRight: '1px solid var(--color-border)'
	},
	'.cm-lineNumbers .cm-gutterElement': { cursor: 'pointer', padding: '0 8px 0 12px' },
	'.cm-lineNumbers .cm-gutterElement:hover': { color: 'var(--color-text)' },
	'.cm-selected-line': { backgroundColor: 'var(--editor-line-selected)' },
	'&.cm-focused > .cm-scroller > .cm-selectionLayer .cm-selectionBackground, .cm-selectionBackground':
		{ backgroundColor: 'var(--editor-selection)' },
	'.cm-blame-gutter .cm-gutterElement': { width: '22rem', overflow: 'hidden' },
	'.cm-blame-entry': {
		display: 'flex',
		gap: '0.5rem',
		color: 'var(--color-text-muted)',
		whiteSpace: 'nowrap',
		overflow: 'hidden',
		textDecoration: 'none',
		padding: '0 0.5rem'
	},
	'.cm-blame-entry:hover': { color: 'var(--color-text)', textDecoration: 'none' },
	'.cm-blame-date': { flex: '0 0 7.5rem', overflow: 'hidden', textOverflow: 'ellipsis' },
	'.cm-blame-author': { flex: '0 0 7rem', overflow: 'hidden', textOverflow: 'ellipsis' },
	'.cm-blame-message': { flex: '1 1 auto', overflow: 'hidden', textOverflow: 'ellipsis' },
	'.cm-panels': { backgroundColor: 'var(--color-bg-subtle)', color: 'var(--color-text)' }
});

export interface CodeViewOptions {
	/** Called when a line number is clicked (`extend` with shift). */
	onLineClick?: (line: number, extend: boolean) => void;
	/** Accessible name of the content. */
	label?: string;
	/** The number shown for the first line (for a snippet of a file); default 1. */
	firstLine?: number;
}

/** Extensions for a read-only code view. */
export function codeViewExtensions(options: CodeViewOptions = {}): Extension[] {
	const offset = (options.firstLine ?? 1) - 1;
	const ext: Extension[] = [
		occurrenceField,
		selectionField,
		blameField,
		lineNumbers({
			formatNumber: (n) => String(n + offset),
			domEventHandlers: {
				mousedown(view, line, event) {
					const n = view.state.doc.lineAt(line.from).number + offset;
					options.onLineClick?.(n, (event as MouseEvent).shiftKey);
					return true;
				}
			}
		}),
		search({ top: true }),
		highlightSelectionMatches(),
		keymap.of(searchKeymap),
		EditorState.readOnly.of(true),
		EditorView.contentAttributes.of({ 'aria-readonly': 'true', 'aria-label': options.label ?? 'File content' }),
		theme
	];
	return ext;
}

/** Scrolls so that `line` (1-based) is near the top third of the view. */
export function scrollToLine(view: EditorView, line: number): void {
	if (line < 1 || line > view.state.doc.lines) return;
	const pos = view.state.doc.line(line).from;
	view.dispatch({ effects: EditorView.scrollIntoView(pos, { y: 'start', yMargin: 80 }) });
}
