/**
 * Client for the compute stream (`GET /.api/compute/stream`). Result types
 * follow `crates/compute/src/result.rs` (`MatchContext` and `Text`).
 */
import { repoPageURL } from '#lib/repo/url.ts';
import { SSEParser, type Alert, type Progress } from './stream.ts';

export interface ComputeLocation {
	offset: number;
	line: number;
	column: number;
}

export interface ComputeRange {
	start: ComputeLocation;
	end: ComputeLocation;
}

export interface ComputeData {
	value: string;
	range: ComputeRange;
}

export interface ComputeMatch {
	value: string;
	range: ComputeRange;
	environment: Record<string, ComputeData>;
}

export interface ComputeMatchContext {
	matches: ComputeMatch[];
	path: string;
	repositoryID: number;
	repository: string;
	/** The commit searched. */
	commit?: string;
}

export interface ComputeText {
	value: string;
	kind: string;
	/** `output.extra` and `replace-in-place`. */
	repositoryID?: number;
	repository?: string;
	/** `replace-in-place`: the rewritten file's commit and path. */
	commit?: string;
	path?: string;
}

export type ComputeResult = ComputeMatchContext | ComputeText;

export function isMatchContext(r: ComputeResult): r is ComputeMatchContext {
	return 'matches' in r;
}

export interface ComputeHandlers {
	onResults?(results: ComputeResult[]): void;
	onProgress?(progress: Progress): void;
	onAlert?(alert: Alert): void;
	onError?(message: string): void;
	onDone?(): void;
}

/** The compute page for `query`. Compute patterns are regexps, so a `patterntype:` is dropped. */
export function computePageURL(query: string): string {
	const q = query
		.replace(/(^|\s)patterntype:\S+/gi, ' ')
		.replace(/\s+/g, ' ')
		.trim();
	return `/compute?${new URLSearchParams({ q }).toString()}`;
}

/** The output values of `results` joined, as the page shows and downloads them. */
export function combinedOutput(results: ComputeResult[]): string {
	return results
		.filter((r): r is ComputeText => !isMatchContext(r) && r.kind === 'output')
		.map((r) => r.value)
		.join('');
}

/**
 * The text Copy and Download give: the joined output values, or for match-only queries one
 * `repo:path:line:match` line per match.
 */
export function exportText(results: ComputeResult[]): string {
	const out = combinedOutput(results);
	if (out) return out;
	return results
		.filter(isMatchContext)
		.flatMap((r) => r.matches.map((m) => `${r.repository}:${r.path}:${m.range.start.line + 1}:${m.value}\n`))
		.join('');
}

/** The file view of a result's file, or null when the result has no file. */
export function resultFileURL(r: ComputeResult): string | null {
	if (!r.repository || !r.path) return null;
	return repoPageURL(r.repository, r.commit || null, 'blob', r.path);
}

/** The raw text of a `replace-in-place` result's original file, or null. */
export function originalFileURL(r: ComputeText): string | null {
	if (r.kind !== 'replace-in-place' || !r.repository || !r.path || !r.commit) return null;
	return repoPageURL(r.repository, r.commit, 'raw', r.path);
}

export function computeURL(query: string): string {
	return `/.api/compute/stream?${new URLSearchParams({ q: query }).toString()}`;
}

/** Runs a compute query, calling the handlers as events arrive. Aborting `signal` stops it. */
export async function streamCompute(
	query: string,
	handlers: ComputeHandlers,
	signal?: AbortSignal,
	doFetch: typeof fetch = fetch
): Promise<void> {
	let res: Response;
	try {
		res = await doFetch(computeURL(query), {
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
		handlers.onError?.(text.trim() || `Compute failed: ${res.status} ${res.statusText}`.trim());
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
				const json = (): unknown => JSON.parse(msg.data);
				switch (msg.event) {
					case 'results':
						handlers.onResults?.(json() as ComputeResult[]);
						break;
					case 'progress':
						handlers.onProgress?.(json() as Progress);
						break;
					case 'alert':
						handlers.onAlert?.(json() as Alert);
						break;
					case 'error':
						handlers.onError?.((json() as { message: string }).message);
						break;
					case 'done':
						done = true;
						handlers.onDone?.();
						break;
				}
			}
		}
	} catch (err) {
		if (signal?.aborted) return;
		handlers.onError?.(`Compute interrupted: ${String(err)}`);
		return;
	}
	if (!done) handlers.onDone?.();
}
