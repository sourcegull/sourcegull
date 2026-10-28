/**
 * Turns match ranges from the stream (byte columns, see `crates/search/src/stream.rs`) into
 * highlighted segments of JavaScript strings.
 */
import type { ChunkMatch, CommitMatch } from './stream.ts';

export interface Segment {
	text: string;
	match: boolean;
}

export interface LineView {
	/** 1-based line number (in the file, or in the message/diff for commits). */
	number: number;
	segments: Segment[];
	/** Diff previews: the line's marker (`+`, `-`, ` `, `@`), else empty. */
	marker?: string;
}

const encoder = new TextEncoder();

/** UTF-16 index of each UTF-8 byte offset of `s` (offsets inside a char map to its start). */
function byteToUtf16(s: string): Uint32Array {
	const bytes = encoder.encode(s).length;
	const map = new Uint32Array(bytes + 1);
	let b = 0;
	for (let i = 0; i < s.length; i++) {
		const cp = s.codePointAt(i) ?? 0;
		const n = cp < 0x80 ? 1 : cp < 0x800 ? 2 : cp < 0x10000 ? 3 : 4;
		for (let k = 0; k < n; k++) map[b + k] = i;
		b += n;
		if (cp >= 0x10000) i++;
	}
	map[bytes] = s.length;
	return map;
}

/** Splits `line` into segments, marking the byte intervals in `ranges` (merged, clamped). */
export function segments(line: string, ranges: [number, number][]): Segment[] {
	if (ranges.length === 0) return [{ text: line, match: false }];
	const map = byteToUtf16(line);
	const max = map.length - 1;
	const spans = ranges
		.map(([a, b]) => [Math.min(Math.max(a, 0), max), Math.min(Math.max(b, 0), max)] as const)
		.filter(([a, b]) => b > a)
		.map(([a, b]) => [map[a] ?? 0, map[b] ?? line.length] as const)
		.sort((x, y) => x[0] - y[0]);
	const out: Segment[] = [];
	let at = 0;
	for (const [a0, b] of spans) {
		const a = Math.max(a0, at);
		if (b <= a) continue;
		if (a > at) out.push({ text: line.slice(at, a), match: false });
		out.push({ text: line.slice(a, b), match: true });
		at = b;
	}
	if (at < line.length || out.length === 0) out.push({ text: line.slice(at), match: false });
	return out;
}

/** The lines of a chunk with its ranges (which may span lines) highlighted. */
export function chunkLines(chunk: ChunkMatch): LineView[] {
	const lines = chunk.content.replace(/\n$/, '').split('\n');
	return lines.map((text, i) => {
		const line = chunk.contentStart.line + i;
		const len = encoder.encode(text).length;
		const ranges: [number, number][] = [];
		for (const r of chunk.ranges) {
			if (r.start.line > line || r.end.line < line) continue;
			const a = r.start.line === line ? r.start.column : 0;
			const b = r.end.line === line ? r.end.column : len;
			ranges.push([a, b]);
		}
		return { number: line + 1, segments: segments(text, ranges) };
	});
}

/**
 * The body of a commit match (message or diff) without its code fence, with the match ranges
 * (`[line, column, length]`, line 0 being the opening fence) highlighted.
 */
export function commitLines(m: CommitMatch): { language: string; lines: LineView[] } {
	const all = m.content.split('\n');
	const language = (all[0] ?? '').replace(/^```/, '');
	const body = all.slice(1, all[all.length - 1] === '```' ? -1 : undefined);
	const diff = language === 'diff';
	const lines = body.map((text, i) => {
		const ranges: [number, number][] = m.ranges
			.filter(([l]) => l === i + 1)
			.map(([, c, n]) => [c, c + n]);
		const view: LineView = { number: i + 1, segments: segments(text, ranges) };
		if (diff) view.marker = text.startsWith('@@') ? '@' : (text[0] ?? ' ');
		return view;
	});
	return { language, lines };
}

/** Groups consecutive chunk lines and drops duplicates where chunks overlap. */
export function mergeChunks(chunks: ChunkMatch[]): LineView[][] {
	const groups: LineView[][] = [];
	let last = -1;
	for (const c of [...chunks].sort((a, b) => a.contentStart.line - b.contentStart.line)) {
		const lines = chunkLines(c).filter((l) => l.number > last);
		if (lines.length === 0) continue;
		const prev = groups[groups.length - 1];
		if (prev && (lines[0]?.number ?? 0) === last + 1) prev.push(...lines);
		else groups.push(lines);
		last = lines[lines.length - 1]?.number ?? last;
	}
	return groups;
}
