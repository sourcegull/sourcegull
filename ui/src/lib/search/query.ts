/**
 * The search query in the UI: the WASM query parser (`crates/query-wasm`, served as
 * `/query.wasm`) for highlighting and diagnostics, and string helpers for the search page
 * (context prefix, filters, URLs, the word under the cursor for autocomplete).
 */

export type PatternType = 'keyword' | 'standard' | 'literal' | 'regexp' | 'structural';

export const PATTERN_TYPES: readonly PatternType[] = [
	'keyword',
	'standard',
	'literal',
	'regexp',
	'structural'
];

export function parsePatternType(s: string | null | undefined): PatternType {
	const v = (s ?? '').toLowerCase();
	return (PATTERN_TYPES as readonly string[]).includes(v) ? (v as PatternType) : 'keyword';
}

export type TokenKind =
	| 'field'
	| 'value'
	| 'predicate'
	| 'negation'
	| 'pattern'
	| 'regexp'
	| 'quoted'
	| 'keyword'
	| 'paren';

/** A highlighted range; offsets in UTF-16 code units. */
export interface QueryToken {
	start: number;
	end: number;
	kind: TokenKind;
}

export interface QueryDiagnostic {
	start: number;
	end: number;
	message: string;
}

export interface QueryAnalysis {
	/** The effective pattern type (a `patterntype:` in the query wins). */
	patternType: PatternType;
	tokens: QueryToken[];
	diagnostics: QueryDiagnostic[];
}

interface RawAnalysis {
	patternType: PatternType;
	tokens: [number, number, TokenKind][];
	diagnostics: QueryDiagnostic[];
}

interface ParserExports {
	memory: WebAssembly.Memory;
	wasm_input(len: number): number;
	wasm_analyze(patternType: number): number;
	wasm_fields(): number;
	wasm_output(): number;
}

/** The query parser running in WebAssembly. */
export class QueryParser {
	readonly #exports: ParserExports;
	readonly #encoder = new TextEncoder();
	readonly #decoder = new TextDecoder();

	constructor(instance: WebAssembly.Instance) {
		this.#exports = instance.exports as unknown as ParserExports;
	}

	#output(len: number): string {
		const ptr = this.#exports.wasm_output();
		return this.#decoder.decode(new Uint8Array(this.#exports.memory.buffer, ptr, len));
	}

	analyze(query: string, patternType: PatternType): QueryAnalysis {
		const bytes = this.#encoder.encode(query);
		const ptr = this.#exports.wasm_input(bytes.length);
		// The memory may have grown in wasm_input: take the buffer afterwards.
		new Uint8Array(this.#exports.memory.buffer, ptr, bytes.length).set(bytes);
		const len = this.#exports.wasm_analyze(PATTERN_TYPES.indexOf(patternType));
		const raw = JSON.parse(this.#output(len)) as RawAnalysis;
		return {
			patternType: raw.patternType,
			tokens: raw.tokens.map(([start, end, kind]) => ({ start, end, kind })),
			diagnostics: raw.diagnostics
		};
	}

	/** Fields with their aliases, canonical name first. */
	fields(): string[][] {
		return JSON.parse(this.#output(this.#exports.wasm_fields())) as string[][];
	}
}

let parser: Promise<QueryParser | null> | undefined;

/**
 * Loads the parser once. Resolves to null when `/query.wasm` is missing (a UI built without
 * `npm run wasm`): the query box then works without highlighting and diagnostics.
 */
export function loadQueryParser(url = '/query.wasm'): Promise<QueryParser | null> {
	parser ??= (async () => {
		try {
			const res = await fetch(url);
			if (!res.ok) return null;
			const { instance } = await WebAssembly.instantiate(await res.arrayBuffer(), {});
			return new QueryParser(instance);
		} catch (err) {
			console.warn('query parser unavailable:', err);
			return null;
		}
	})();
	return parser;
}

/** Fields offered by autocomplete when the parser is not loaded (canonical names). */
export const FALLBACK_FIELDS: readonly string[] = [
	'repo',
	'file',
	'lang',
	'content',
	'type',
	'case',
	'context',
	'select',
	'count',
	'timeout',
	'fork',
	'archived',
	'visibility',
	'rev',
	'author',
	'committer',
	'message',
	'before',
	'after',
	'patterntype',
	'index',
	'rule'
];

/** Values offered for fields with a fixed set. */
export const FIELD_VALUES: Readonly<Record<string, readonly string[]>> = {
	type: ['repo', 'file', 'path', 'symbol', 'diff', 'commit'],
	case: ['yes', 'no'],
	fork: ['yes', 'no', 'only'],
	archived: ['yes', 'no', 'only'],
	visibility: ['any', 'public', 'private'],
	index: ['yes', 'no', 'only'],
	count: ['all', '1000', '10000'],
	patterntype: [...PATTERN_TYPES],
	select: [
		'repo',
		'file',
		'file.directory',
		'content',
		'symbol',
		'symbol.function',
		'symbol.class',
		'symbol.method',
		'commit',
		'commit.diff.added',
		'commit.diff.removed'
	],
	lang: [
		'c',
		'c++',
		'c#',
		'css',
		'go',
		'html',
		'java',
		'javascript',
		'json',
		'kotlin',
		'markdown',
		'php',
		'python',
		'ruby',
		'rust',
		'scala',
		'shell',
		'sql',
		'swift',
		'tsx',
		'typescript',
		'yaml'
	],
	before: ['yesterday', '"1 week ago"', '"1 month ago"', '2024-01-01'],
	after: ['yesterday', '"1 week ago"', '"1 month ago"', '2024-01-01']
};

const CONTEXT_RE = /(^|\s)context:("(?:[^"\\]|\\.)*"|\S+)(?=\s|$)/;

/**
 * Splits the first top-level `context:` off a query: the search page shows it in the context
 * picker and the rest in the query box.
 */
export function splitContext(query: string): { context: string | null; rest: string } {
	const m = CONTEXT_RE.exec(query);
	if (!m) return { context: null, rest: query.trim() };
	const start = m.index + (m[1]?.length ?? 0);
	const rest = (query.slice(0, start) + query.slice(m.index + m[0].length)).replace(/\s+/g, ' ');
	let context = m[2] ?? '';
	if (context.startsWith('"')) context = context.slice(1, -1).replace(/\\(.)/g, '$1');
	return { context, rest: rest.trim() };
}

/** `context:<spec> <query>`; the query alone without a context or when it names one itself. */
export function withContext(query: string, context: string | null): string {
	const q = query.trim();
	if (!context || CONTEXT_RE.test(q)) return q;
	return q ? `context:${context} ${q}` : `context:${context}`;
}

/** Adds a filter (a query fragment such as `lang:go`) unless the query has it already. */
export function appendFilter(query: string, filter: string): string {
	const q = query.trim();
	const words = q.split(/\s+/);
	if (words.includes(filter)) return q;
	return q ? `${q} ${filter}` : filter;
}

/** Removes a filter added with {@link appendFilter}. */
export function removeFilter(query: string, filter: string): string {
	return query
		.trim()
		.split(/\s+/)
		.filter((w) => w !== filter)
		.join(' ');
}

/**
 * The query with a `patternType:` (saved searches require one), unless it has one already.
 * Searches in the URL keep the pattern type in a parameter instead.
 */
export function withPatternType(query: string, patternType: PatternType): string {
	const q = query.trim();
	return /(^|\s)patterntype:/i.test(q) ? q : `${q} patternType:${patternType}`;
}

/** `/search?q=…&patternType=…`, taking a `patternType:` out of the query into the parameter. */
export function searchQueryURL(query: string): string {
	const m = /(^|\s)patterntype:(\S+)/i.exec(query);
	if (!m) return searchURL(query);
	const rest = (query.slice(0, m.index) + query.slice(m.index + m[0].length)).replace(/\s+/g, ' ').trim();
	return searchURL(rest, parsePatternType(m[2]));
}

/** `/search?q=…&patternType=…`. */
export function searchURL(query: string, patternType: PatternType = 'keyword'): string {
	const p = new URLSearchParams({ q: query });
	if (patternType !== 'keyword') p.set('patternType', patternType);
	return `/search?${p.toString()}`;
}

/** The word around the cursor, for autocomplete. */
export type CursorWord =
	/** In a field's value: `field` is the name as typed (lower case), `from` where the value starts. */
	| { kind: 'value'; field: string; from: number; prefix: string }
	/** In a bare word (a pattern or a field name being typed). */
	| { kind: 'word'; from: number; prefix: string };

/** Classifies the text before `pos` (whitespace ends a word; quotes are not tracked). */
export function wordAt(text: string, pos: number): CursorWord {
	let from = pos;
	while (from > 0 && !/\s|\(/.test(text[from - 1] ?? '')) from--;
	const word = text.slice(from, pos);
	const neg = word.startsWith('-') ? 1 : 0;
	const colon = word.indexOf(':');
	if (colon > neg && /^[A-Za-z.]+$/.test(word.slice(neg, colon))) {
		return {
			kind: 'value',
			field: word.slice(neg, colon).toLowerCase(),
			from: from + colon + 1,
			prefix: word.slice(colon + 1)
		};
	}
	return { kind: 'word', from: from + neg, prefix: word.slice(neg) };
}

/** Quotes a context spec or repo name for a query when it holds spaces or quotes. */
export function quoteValue(v: string): string {
	return /[\s"]/.test(v) ? `"${v.replace(/["\\]/g, '\\$&')}"` : v;
}

/** `^…$` around a regexp-escaped repository name, as `repo:` filters use. */
export function exactRepo(name: string): string {
	return `^${name.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}$`;
}
