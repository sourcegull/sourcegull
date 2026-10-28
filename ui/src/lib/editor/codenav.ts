/**
 * Code navigation in the file view: hovering an identifier shows its hover text and
 * links to its definition and references; Ctrl/Cmd-click goes to the definition.
 */
import type { Extension } from '@codemirror/state';
import { EditorView, hoverTooltip, type Tooltip } from '@codemirror/view';
import { renderMarkdown } from '#lib/codenav/markdown.ts';
import { provenanceLabel, type SymbolInfo } from '#lib/codenav/queries.ts';

export interface CodeNavHandlers {
	/** The symbol at a 0-based line and UTF-16 character, or null. */
	symbolInfo(line: number, character: number): Promise<SymbolInfo | null>;
	/** The page of the symbol's definition, or null without one. */
	definitionURL(info: SymbolInfo): string | null;
	/** Navigates to a URL within the app. */
	goTo(url: string): void;
	/** Opens the usages panel for the symbol. */
	findReferences(info: SymbolInfo): void;
}

const IDENT = /[\p{L}\p{N}_$]/u;

/** The identifier around document offset `pos` as [from, to), or null. */
export function wordAt(text: string, pos: number): [number, number] | null {
	if (pos < 0 || pos > text.length) return null;
	let from = pos;
	let to = pos;
	while (from > 0 && IDENT.test(text[from - 1] ?? '')) from--;
	while (to < text.length && IDENT.test(text[to] ?? '')) to++;
	if (from === to || /^\p{N}/u.test(text.slice(from, to))) return null;
	return [from, to];
}

function lookup(
	view: EditorView,
	handlers: CodeNavHandlers,
	cache: Map<string, Promise<SymbolInfo | null>>,
	pos: number
): { from: number; info: Promise<SymbolInfo | null> } | null {
	const line = view.state.doc.lineAt(pos);
	const word = wordAt(line.text, pos - line.from);
	if (!word) return null;
	const key = `${line.number - 1}:${word[0]}`;
	let info = cache.get(key);
	if (!info) {
		info = handlers.symbolInfo(line.number - 1, word[0]).catch(() => null);
		cache.set(key, info);
	}
	return { from: line.from + word[0], info };
}

function hoverCard(info: SymbolInfo, handlers: CodeNavHandlers): HTMLElement {
	const card = document.createElement('div');
	card.className = 'cm-codenav-hover';
	card.setAttribute('role', 'tooltip');
	if (info.hover) {
		const body = document.createElement('div');
		body.className = 'cm-codenav-body';
		body.append(renderMarkdown(info.hover));
		card.append(body);
	} else {
		const none = document.createElement('div');
		none.className = 'cm-codenav-body cm-codenav-empty';
		none.textContent = 'No definition found.';
		card.append(none);
	}
	const actions = document.createElement('div');
	actions.className = 'cm-codenav-actions';
	const url = handlers.definitionURL(info);
	if (url) {
		const a = document.createElement('a');
		a.href = url;
		a.className = 'cm-codenav-action';
		a.textContent = 'Go to definition';
		a.addEventListener('click', (e) => {
			if (e.metaKey || e.ctrlKey || e.shiftKey || e.button !== 0) return;
			e.preventDefault();
			handlers.goTo(url);
		});
		actions.append(a);
	}
	const refs = document.createElement('button');
	refs.type = 'button';
	refs.className = 'cm-codenav-action';
	refs.textContent = 'Find references';
	refs.addEventListener('click', () => handlers.findReferences(info));
	actions.append(refs);
	const badge = document.createElement('span');
	badge.className = 'cm-codenav-badge';
	badge.textContent = provenanceLabel(info.provenance);
	badge.title =
		info.provenance === 'SEARCH_BASED'
			? 'Found by text search; results may be imprecise.'
			: `From ${provenanceLabel(info.provenance).toLowerCase()} code graph data.`;
	actions.append(badge);
	card.append(actions);
	return card;
}

/** The code navigation extension; one per file view. */
export function codeNavExtension(handlers: CodeNavHandlers): Extension {
	const cache = new Map<string, Promise<SymbolInfo | null>>();
	const tooltips = hoverTooltip(
		async (view, pos): Promise<Tooltip | null> => {
			const hit = lookup(view, handlers, cache, pos);
			if (!hit) return null;
			const info = await hit.info;
			if (!info) return null;
			const doc = view.state.doc;
			const line = doc.line(Math.min(info.range.start.line + 1, doc.lines));
			const from = Math.min(line.from + info.range.start.character, line.to);
			const to = Math.min(line.from + info.range.end.character, line.to);
			return {
				pos: from,
				end: to,
				above: true,
				create: () => ({ dom: hoverCard(info, handlers) })
			};
		},
		{ hoverTime: 250 }
	);
	const clicks = EditorView.domEventHandlers({
		mousedown(event, view) {
			if (!(event.metaKey || event.ctrlKey) || event.button !== 0) return false;
			const pos = view.posAtCoords({ x: event.clientX, y: event.clientY });
			if (pos === null) return false;
			const hit = lookup(view, handlers, cache, pos);
			if (!hit) return false;
			event.preventDefault();
			void hit.info.then((info) => {
				const url = info && handlers.definitionURL(info);
				if (url) handlers.goTo(url);
			});
			return true;
		}
	});
	return [tooltips, clicks, theme];
}

const theme = EditorView.theme({
	'.cm-tooltip.cm-tooltip-hover': {
		border: '1px solid var(--color-border)',
		borderRadius: 'var(--radius)',
		backgroundColor: 'var(--color-surface)',
		boxShadow: 'var(--shadow-lg)'
	},
	'.cm-codenav-hover': {
		maxWidth: 'min(40rem, 90vw)',
		fontFamily: 'var(--font-sans)',
		fontSize: '13px',
		color: 'var(--color-text)'
	},
	'.cm-codenav-body': {
		maxHeight: '20rem',
		overflow: 'auto',
		padding: '0.5rem 0.75rem'
	},
	'.cm-codenav-body pre': {
		margin: '0',
		fontFamily: 'var(--font-mono)',
		whiteSpace: 'pre-wrap',
		wordBreak: 'break-word'
	},
	'.cm-codenav-body p': { margin: '0.4rem 0', overflowWrap: 'anywhere' },
	'.cm-codenav-body hr': {
		border: 'none',
		borderTop: '1px solid var(--color-border)',
		margin: '0.5rem -0.75rem'
	},
	'.cm-codenav-body code': { fontFamily: 'var(--font-mono)' },
	'.cm-codenav-empty': { color: 'var(--color-text-muted)' },
	'.cm-codenav-actions': {
		display: 'flex',
		alignItems: 'center',
		gap: '0.25rem',
		borderTop: '1px solid var(--color-border)',
		padding: '0.25rem 0.5rem',
		backgroundColor: 'var(--color-bg-subtle)'
	},
	'.cm-codenav-action': {
		font: 'inherit',
		color: 'var(--color-primary)',
		background: 'none',
		border: 'none',
		padding: '0.25rem 0.5rem',
		borderRadius: 'var(--radius)',
		cursor: 'pointer',
		textDecoration: 'none'
	},
	'.cm-codenav-action:hover': { backgroundColor: 'var(--color-bg-muted)' },
	'.cm-codenav-badge': {
		marginLeft: 'auto',
		fontSize: '11px',
		color: 'var(--color-text-muted)',
		border: '1px solid var(--color-border)',
		borderRadius: '999px',
		padding: '0 0.5rem'
	}
});
