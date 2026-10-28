/**
 * Streaming blame (`GET /.api/blame/{repo}@{rev}/-/stream/{path}`): server-sent events, `hunk`
 * events carrying a JSON array of hunks, then `done`.
 */

import { encodePath } from './url.ts';

export interface BlameHunk {
	commitID: string;
	/** 1-based first line. */
	startLine: number;
	/** Exclusive. */
	endLine: number;
	author: { Name: string; Email: string; Date: string };
	message: string;
	filename: string;
	commit: { parents: string[]; url: string };
}

export interface SSEEvent {
	event: string;
	data: string;
}

/** Incremental `text/event-stream` parser: feed chunks, get complete events back. */
export class SSEParser {
	private buffer = '';
	private event = '';
	private data: string[] = [];

	push(chunk: string): SSEEvent[] {
		this.buffer += chunk;
		const out: SSEEvent[] = [];
		let nl: number;
		while ((nl = this.buffer.search(/\r\n|\r|\n/)) >= 0) {
			const line = this.buffer.slice(0, nl);
			const len = this.buffer.startsWith('\r\n', nl) ? 2 : 1;
			this.buffer = this.buffer.slice(nl + len);
			if (line === '') {
				if (this.data.length > 0 || this.event !== '') {
					out.push({ event: this.event || 'message', data: this.data.join('\n') });
				}
				this.event = '';
				this.data = [];
				continue;
			}
			if (line.startsWith(':')) continue; // comment / keep-alive
			const colon = line.indexOf(':');
			const field = colon >= 0 ? line.slice(0, colon) : line;
			let value = colon >= 0 ? line.slice(colon + 1) : '';
			if (value.startsWith(' ')) value = value.slice(1);
			if (field === 'event') this.event = value;
			else if (field === 'data') this.data.push(value);
		}
		return out;
	}
}

/** The blame stream URL of a file at a revision. */
export function blameStreamURL(repo: string, rev: string, path: string): string {
	return `/.api/blame/${encodePath(repo)}@${encodePath(rev)}/-/stream/${encodePath(path)}`;
}

/**
 * Streams blame hunks, calling `onHunks` as batches arrive. Resolves when the server sends
 * `done`; rejects on HTTP or stream errors (or with an AbortError when `signal` fires).
 */
export async function streamBlame(
	url: string,
	onHunks: (hunks: BlameHunk[]) => void,
	signal?: AbortSignal,
	doFetch: typeof fetch = fetch
): Promise<void> {
	const resp = await doFetch(url, {
		credentials: 'same-origin',
		headers: { Accept: 'text/event-stream' },
		signal
	});
	if (!resp.ok || !resp.body) {
		let message = `Blame failed: ${resp.status}`;
		try {
			const body = (await resp.json()) as { error?: string };
			if (body.error) message = body.error;
		} catch {
			// not JSON
		}
		throw new Error(message);
	}
	const reader = resp.body.pipeThrough(new TextDecoderStream()).getReader();
	const parser = new SSEParser();
	for (;;) {
		const { value, done } = await reader.read();
		if (done) return;
		for (const ev of parser.push(value)) {
			if (ev.event === 'hunk') onHunks(JSON.parse(ev.data) as BlameHunk[]);
			else if (ev.event === 'done') return;
			else if (ev.event === 'error') {
				const body = JSON.parse(ev.data) as { message?: string };
				throw new Error(body.message ?? 'Blame failed');
			}
		}
	}
}

/** A short relative date ("3 days ago") for blame and history lists. */
export function relativeDate(iso: string, now: Date = new Date()): string {
	const then = new Date(iso);
	const secs = Math.round((now.getTime() - then.getTime()) / 1000);
	if (!Number.isFinite(secs)) return iso;
	const units: [number, string][] = [
		[365 * 24 * 3600, 'year'],
		[30 * 24 * 3600, 'month'],
		[7 * 24 * 3600, 'week'],
		[24 * 3600, 'day'],
		[3600, 'hour'],
		[60, 'minute']
	];
	for (const [size, name] of units) {
		const n = Math.floor(Math.abs(secs) / size);
		if (n >= 1) return secs >= 0 ? `${n} ${name}${n === 1 ? '' : 's'} ago` : `in ${n} ${name}${n === 1 ? '' : 's'}`;
	}
	return 'just now';
}
