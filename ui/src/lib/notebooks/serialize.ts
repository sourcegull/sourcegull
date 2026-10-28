/**
 * Notebook export and import as `.snb.md`: Markdown in which query blocks
 * are ```` ```sourcegull ```` fences and file and symbol blocks are bare links to file pages:
 *
 *   - Markdown block: its text.
 *   - Query block: ```` ```sourcegull\n<query>\n``` ````.
 *   - File block: `<origin>/<repo>[@<rev>]/-/blob/<path>[?L<start>[-<end>]]` (1-based lines).
 *   - Symbol block: the definition's file URL with its range (`?L<line>:<col>-<line>:<col>`) and
 *     `#symbolName=…&symbolContainerName=…&symbolKind=…&lineContext=…`.
 *
 * Blocks are separated by a blank line. On import, a paragraph that is only such a link becomes
 * a file block (a symbol block with `symbolName` in the hash), a `sourcegull` fence a query
 * block, and everything between them Markdown blocks. Links with the line range in the hash
 * (`#L10-20`, this UI's style) are understood too.
 */
import { parseLineRange, parseRepoRoute, repoPageURL } from '#lib/repo/url.ts';

/** Lines `startLine..endLine`, 0-based, end exclusive (the GraphQL `FileBlockLineRange`). */
export interface BlockLineRange {
	startLine: number;
	endLine: number;
}

export interface FileBlockInput {
	repositoryName: string;
	/** `''`: the default branch. */
	revision: string;
	filePath: string;
	lineRange: BlockLineRange | null;
}

export interface SymbolBlockInput {
	repositoryName: string;
	/** `''`: the default branch. */
	revision: string;
	filePath: string;
	symbolName: string;
	symbolContainerName: string;
	/** A GraphQL `SymbolKind`, e.g. `FUNCTION`. */
	symbolKind: string;
	lineContext: number;
}

export type BlockInput =
	| { type: 'md'; input: { text: string } }
	| { type: 'query'; input: { query: string } }
	| { type: 'file'; input: FileBlockInput }
	| { type: 'symbol'; input: SymbolBlockInput };

export type Block = BlockInput & { id: string };

/** Where a symbol block's symbol was found when viewed: exported in its link. */
export interface ResolvedSymbol {
	revision: string;
	filePath: string;
	/** 1-based line and column of the name's start and end. */
	range: { startLine: number; startCol: number; endLine: number; endCol: number };
}

export const DEFAULT_LINE_CONTEXT = 3;

/** `124` for a single line, else `124-321` (1-based, inclusive). */
export function serializeLineRange(r: BlockLineRange | null): string {
	if (!r) return '';
	return r.startLine + 1 >= r.endLine ? `${r.startLine + 1}` : `${r.startLine + 1}-${r.endLine}`;
}

/** Parses `124` or `124-321` (1-based, inclusive); null when malformed. */
export function parseBlockLineRange(value: string): BlockLineRange | null {
	const m = /^\s*(\d+)\s*(?:-\s*(\d+)\s*)?$/.exec(value);
	if (!m) return null;
	const start = Number(m[1]);
	const end = m[2] === undefined ? start : Number(m[2]);
	if (start < 1 || end < start) return null;
	return { startLine: start - 1, endLine: end };
}

/** The absolute URL of a file page, with an optional 1-based `?L…` range. */
function blobURL(origin: string, repo: string, rev: string, path: string, range: string): string {
	const base = origin.replace(/\/+$/, '') + repoPageURL(repo, rev || null, 'blob', path);
	return range ? `${base}?L${range}` : base;
}

/** The `.snb.md` text of one block. */
export function serializeBlock(block: BlockInput, origin: string, resolved?: ResolvedSymbol | null): string {
	switch (block.type) {
		case 'md':
			return block.input.text.trimEnd();
		case 'query':
			return '```sourcegull\n' + block.input.query + '\n```';
		case 'file': {
			const f = block.input;
			return blobURL(origin, f.repositoryName, f.revision, f.filePath, serializeLineRange(f.lineRange));
		}
		case 'symbol': {
			const s = block.input;
			const r = resolved?.range;
			const range = r ? `${r.startLine}:${r.startCol}-${r.endLine}:${r.endCol}` : '';
			const url = blobURL(
				origin,
				s.repositoryName,
				resolved?.revision ?? s.revision,
				resolved?.filePath ?? s.filePath,
				range
			);
			const params = new URLSearchParams([
				['symbolName', s.symbolName],
				['symbolContainerName', s.symbolContainerName],
				['symbolKind', s.symbolKind],
				['lineContext', String(s.lineContext)]
			]);
			return `${url}#${params.toString()}`;
		}
	}
}

/** A whole notebook as `.snb.md`: blocks separated by blank lines. */
export function serializeNotebook(
	blocks: readonly Block[],
	origin: string,
	resolved: ReadonlyMap<string, ResolvedSymbol> = new Map()
): string {
	const parts = blocks.map((b) => serializeBlock(b, origin, resolved.get(b.id))).filter((s) => s !== '');
	return parts.length > 0 ? parts.join('\n\n') + '\n' : '';
}

/** The export file name of a notebook titled `title`. */
export function notebookFileName(title: string): string {
	return title.replace(/[^\da-z]/gi, '_').replace(/_+/g, '_') + '.snb.md';
}

/** The notebook title for an imported file name: the name without `.snb.md` (or `.md`). */
export function titleFromFileName(name: string): string {
	return name.replace(/\.snb\.md$/i, '').replace(/\.md$/i, '').trim() || 'Imported notebook';
}

interface ParsedBlobURL {
	repo: string;
	rev: string;
	path: string;
	/** 1-based, inclusive. */
	range: { start: number; end: number } | null;
	hash: URLSearchParams;
}

/** Parses an absolute link to a file page; null for anything else. */
export function parseBlobURL(s: string): ParsedBlobURL | null {
	let url: URL;
	try {
		url = new URL(s.trim());
	} catch {
		return null;
	}
	if (url.protocol !== 'http:' && url.protocol !== 'https:') return null;
	const route = parseRepoRoute(url.pathname);
	if (!route || route.kind !== 'blob' || !route.path) return null;
	const fromSearch = url.search
		.slice(1)
		.split('&')
		.map((p) => {
			try {
				return parseLineRange(decodeURIComponent(p));
			} catch {
				return null;
			}
		})
		.find((r) => r !== null);
	const range = fromSearch ?? parseLineRange(url.hash);
	return {
		repo: route.repo,
		rev: route.rev ?? '',
		path: route.path,
		range,
		hash: new URLSearchParams(url.hash.replace(/^#/, ''))
	};
}

/** The block a link stands for: a symbol block when it names a symbol, else a file block. */
export function blockFromURL(s: string): BlockInput | null {
	const p = parseBlobURL(s);
	if (!p) return null;
	const symbolName = p.hash.get('symbolName');
	if (symbolName) {
		const lc = Number.parseInt(p.hash.get('lineContext') ?? '', 10);
		return {
			type: 'symbol',
			input: {
				repositoryName: p.repo,
				revision: p.rev,
				filePath: p.path,
				symbolName,
				symbolContainerName: p.hash.get('symbolContainerName') ?? '',
				symbolKind: p.hash.get('symbolKind') || 'UNKNOWN',
				lineContext: Number.isNaN(lc) || lc < 0 ? DEFAULT_LINE_CONTEXT : lc
			}
		};
	}
	return {
		type: 'file',
		input: {
			repositoryName: p.repo,
			revision: p.rev,
			filePath: p.path,
			lineRange: p.range ? { startLine: p.range.start - 1, endLine: p.range.end } : null
		}
	};
}

const BLANK = /^\s*$/;
const HEADING = /^ {0,3}#{1,6}(\s|$)/;
const FENCE = /^ {0,3}(`{3,}|~{3,})\s*([^\s`]*)/;

/** Splits into lines that keep their line break. */
function lines(s: string): string[] {
	return s.match(/[^\n]*\n|[^\n]+$/g) ?? [];
}

/** The URL of a line that is only a link (`url` or `<url>`), or null. */
function lineLink(line: string): string | null {
	const t = line.trim();
	const m = /^<(\S+)>$/.exec(t);
	const url = m ? m[1]! : t;
	return /^https?:\/\/\S+$/.test(url) ? url : null;
}

/** Parses `.snb.md` text into blocks (without IDs). */
export function markdownToBlocks(markdown: string): BlockInput[] {
	const src = lines(markdown.replace(/\r\n?/g, '\n'));
	const out: BlockInput[] = [];
	let raw = '';
	const flush = (): void => {
		// Trailing blank lines of Markdown blocks are dropped, so that export → import is an
		// identity.
		const text = raw.trimStart().replace(/\s+$/, '');
		if (text) out.push({ type: 'md', input: { text } });
		raw = '';
	};
	// Whether the current line starts a paragraph (a link alone in one is a file block).
	let boundary = true;
	for (let i = 0; i < src.length; i++) {
		const line = src[i]!;
		const fence = FENCE.exec(line);
		if (fence) {
			const marker = fence[1]!;
			const lang = fence[2] ?? '';
			let j = i + 1;
			const close = new RegExp(`^ {0,3}${marker[0] === '`' ? '`' : '~'}{${marker.length},}\\s*$`);
			while (j < src.length && !close.test(src[j]!)) j++;
			const end = Math.min(j, src.length - 1);
			if (lang === 'sourcegull') {
				flush();
				const body = src.slice(i + 1, j).join('');
				out.push({ type: 'query', input: { query: body.replace(/\n$/, '') } });
			} else {
				raw += src.slice(i, end + 1).join('');
			}
			i = end;
			boundary = true;
			continue;
		}
		const link = boundary ? lineLink(line) : null;
		const next = src[i + 1];
		if (link && (next === undefined || BLANK.test(next))) {
			const block = blockFromURL(link);
			if (block) {
				flush();
				out.push(block);
				boundary = true;
				continue;
			}
		}
		raw += line;
		boundary = BLANK.test(line) || HEADING.test(line);
	}
	flush();
	return out;
}
