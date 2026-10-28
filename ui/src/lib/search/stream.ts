/**
 * Client for the streaming search API (`GET /.api/search/stream`). The types
 * follow the wire format in `crates/search/src/stream.rs`.
 *
 * Uses `fetch` and parses Server-Sent Events itself rather than `EventSource`, which reconnects on
 * its own (re-running the search) and cannot be cancelled cleanly.
 */
import type { PatternType } from './query.ts';

export interface Location {
	offset: number;
	line: number;
	column: number;
}

export interface Range {
	start: Location;
	end: Location;
}

export interface ChunkMatch {
	content: string;
	contentStart: Location;
	ranges: Range[];
}

interface FileMatchBase {
	path: string;
	repositoryID: number;
	repository: string;
	commit: string;
	branches?: string[];
	language?: string;
}

export interface ContentMatch extends FileMatchBase {
	type: 'content';
	chunkMatches?: ChunkMatch[];
}

export interface PathMatch extends FileMatchBase {
	type: 'path';
	pathMatches?: Range[];
}

export interface SymbolInfo {
	url: string;
	name: string;
	containerName: string;
	kind: string;
	line: number;
}

export interface SymbolMatch extends FileMatchBase {
	type: 'symbol';
	symbols: SymbolInfo[];
}

export interface CommitMatch {
	type: 'commit';
	label: string;
	url: string;
	detail: string;
	repositoryID: number;
	repository: string;
	oid: string;
	message: string;
	authorName: string;
	authorDate: string;
	committerName: string;
	committerDate: string;
	/** The message or diff in a Markdown code fence. */
	content: string;
	/** `[line, column, length]` within `content`. */
	ranges: [number, number, number][];
}

export interface RepoMatch {
	type: 'repo';
	repositoryID: number;
	repository: string;
	description?: string;
	/** Omitted when false. */
	fork?: boolean;
	archived?: boolean;
	private?: boolean;
}

/** An owner of matched files (`select:file.owners`); `user` when it is a user. */
export interface PersonMatch {
	type: 'person';
	handle: string;
	email: string;
	user?: { username: string; displayName: string; avatarURL: string };
}

/** A team owning matched files; `name` is empty for a GitLab group with no team. */
export interface TeamMatch {
	type: 'team';
	handle: string;
	email: string;
	name: string;
	displayName: string;
}

export type SearchMatch =
	| ContentMatch
	| PathMatch
	| SymbolMatch
	| CommitMatch
	| RepoMatch
	| PersonMatch
	| TeamMatch;

export interface Skipped {
	reason: string;
	title: string;
	message: string;
	severity: 'info' | 'warn';
	/** A query expression that avoids the skip, e.g. `timeout:2m`. */
	suggested?: { title: string; queryExpression: string };
}

export interface Progress {
	done: boolean;
	repositoriesCount?: number;
	matchCount: number;
	durationMs: number;
	skipped: Skipped[];
}

export interface Filter {
	value: string;
	label: string;
	count: number;
	/** Whether `count` is exact. */
	exhaustive: boolean;
	kind: string;
}

export interface Alert {
	title: string;
	description?: string;
	proposedQueries: { description?: string; query: string }[];
}

export interface SearchHandlers {
	onMatches?(matches: SearchMatch[]): void;
	onProgress?(progress: Progress): void;
	onFilters?(filters: Filter[]): void;
	onAlert?(alert: Alert): void;
	onError?(message: string): void;
	onDone?(): void;
}

export interface SSEMessage {
	event: string;
	data: string;
}

/** Incremental Server-Sent Events parser (`event:` and `data:` lines; comments ignored). */
export class SSEParser {
	#buffer = '';
	#event = '';
	#data: string[] = [];

	/** Feeds a chunk and returns the messages it completed. */
	push(chunk: string): SSEMessage[] {
		this.#buffer += chunk;
		const out: SSEMessage[] = [];
		for (;;) {
			const nl = this.#buffer.search(/\r\n|\r|\n/);
			if (nl < 0) break;
			const sep = this.#buffer.startsWith('\r\n', nl) ? 2 : 1;
			const line = this.#buffer.slice(0, nl);
			this.#buffer = this.#buffer.slice(nl + sep);
			if (line === '') {
				if (this.#data.length > 0 || this.#event) {
					out.push({ event: this.#event || 'message', data: this.#data.join('\n') });
				}
				this.#event = '';
				this.#data = [];
				continue;
			}
			if (line.startsWith(':')) continue;
			const colon = line.indexOf(':');
			const field = colon < 0 ? line : line.slice(0, colon);
			let value = colon < 0 ? '' : line.slice(colon + 1);
			if (value.startsWith(' ')) value = value.slice(1);
			if (field === 'event') this.#event = value;
			else if (field === 'data') this.#data.push(value);
		}
		return out;
	}
}

export interface SearchParams {
	query: string;
	patternType: PatternType;
	/** Results to send (the stream's display limit). */
	display?: number;
	maxLineLen?: number;
}

export function streamURL(p: SearchParams): string {
	const q = new URLSearchParams({ q: p.query, v: 'V3', t: p.patternType, cm: 't' });
	if (p.display !== undefined) q.set('display', String(p.display));
	if (p.maxLineLen !== undefined) q.set('max-line-len', String(p.maxLineLen));
	return `/.api/search/stream?${q.toString()}`;
}

/** Dispatches one SSE message to the handlers. */
export function dispatch(msg: SSEMessage, h: SearchHandlers): void {
	const json = (): unknown => JSON.parse(msg.data);
	switch (msg.event) {
		case 'matches':
			h.onMatches?.(json() as SearchMatch[]);
			break;
		case 'progress':
			h.onProgress?.(json() as Progress);
			break;
		case 'filters':
			h.onFilters?.(json() as Filter[]);
			break;
		case 'alert':
			h.onAlert?.(json() as Alert);
			break;
		case 'error':
			h.onError?.((json() as { message: string }).message);
			break;
		case 'done':
			h.onDone?.();
			break;
	}
}

/**
 * Runs a search and calls the handlers as events arrive. Resolves when the stream ends; aborting
 * `signal` stops it (and resolves without calling `onDone`).
 */
export async function streamSearch(
	params: SearchParams,
	handlers: SearchHandlers,
	signal?: AbortSignal,
	doFetch: typeof fetch = fetch
): Promise<void> {
	let res: Response;
	try {
		res = await doFetch(streamURL(params), {
			credentials: 'same-origin',
			headers: { Accept: 'text/event-stream' },
			signal
		});
	} catch (err) {
		if (signal?.aborted) return;
		handlers.onError?.(`Network error: ${String(err)}`);
		return;
	}
	if (!res.ok || !res.body) {
		const text = await res.text().catch(() => '');
		handlers.onError?.(text.trim() || `Search failed: ${res.status} ${res.statusText}`.trim());
		return;
	}
	const reader = res.body.pipeThrough(new TextDecoderStream()).getReader();
	const parser = new SSEParser();
	let done = false;
	try {
		for (;;) {
			const { value, done: end } = await reader.read();
			if (end) break;
			for (const msg of parser.push(value)) {
				if (msg.event === 'done') done = true;
				dispatch(msg, handlers);
			}
		}
	} catch (err) {
		if (signal?.aborted) return;
		handlers.onError?.(`Search interrupted: ${String(err)}`);
		return;
	}
	if (!done) handlers.onDone?.();
}

/** A stable key for a match (for keyed lists). */
export function matchKey(m: SearchMatch, index: number): string {
	switch (m.type) {
		case 'commit':
			return `c:${m.repositoryID}:${m.oid}:${index}`;
		case 'repo':
			return `r:${m.repositoryID}`;
		case 'person':
			return `p:${m.handle}:${m.email}`;
		case 'team':
			return `t:${m.handle}`;
		default:
			return `${m.type[0]}:${m.repositoryID}:${m.commit}:${m.path}:${index}`;
	}
}

/** Number of result units in a match, as counted for "N results". */
export function matchCount(m: SearchMatch): number {
	switch (m.type) {
		case 'content':
			return Math.max(
				1,
				(m.chunkMatches ?? []).reduce((n, c) => n + c.ranges.length, 0)
			);
		case 'symbol':
			return Math.max(1, m.symbols.length);
		default:
			return 1;
	}
}
