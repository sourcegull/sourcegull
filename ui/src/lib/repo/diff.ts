/** Unified-diff hunk rendering helpers. */
import type { FileDiffNode } from './queries.ts';

export interface DiffLine {
	kind: 'context' | 'added' | 'deleted' | 'note';
	/** Line number on the old side (null for added lines and notes). */
	oldLine: number | null;
	/** Line number on the new side (null for deleted lines and notes). */
	newLine: number | null;
	/** The text without the prefix character. */
	text: string;
}

/**
 * Splits a hunk body (lines prefixed with ` `, `+`, `-` or `\`) into numbered lines, starting at
 * the hunk's old and new start lines.
 */
export function diffLines(body: string, oldStart: number, newStart: number): DiffLine[] {
	const out: DiffLine[] = [];
	let o = oldStart;
	let n = newStart;
	const lines = body.endsWith('\n') ? body.slice(0, -1).split('\n') : body.split('\n');
	if (body === '') return out;
	for (const raw of lines) {
		const prefix = raw.charAt(0);
		const text = raw.slice(1);
		switch (prefix) {
			case '+':
				out.push({ kind: 'added', oldLine: null, newLine: n++, text });
				break;
			case '-':
				out.push({ kind: 'deleted', oldLine: o++, newLine: null, text });
				break;
			case '\\':
				out.push({ kind: 'note', oldLine: null, newLine: null, text: raw });
				break;
			default:
				out.push({ kind: 'context', oldLine: o++, newLine: n++, text });
		}
	}
	return out;
}

/** Human-readable byte size. */
export function formatBytes(n: number): string {
	if (n < 1024) return `${n} B`;
	const units = ['KB', 'MB', 'GB'];
	let v = n;
	let unit = 'B';
	for (const u of units) {
		if (v < 1024) break;
		v /= 1024;
		unit = u;
	}
	return `${v < 10 ? v.toFixed(1) : Math.round(v)} ${unit}`;
}

type DiffOp = { kind: 'context' | 'added' | 'deleted'; text: string };

/** Edit distance above which `textLineDiff` gives up and replaces the whole changed region. */
const MAX_EDITS = 1000;

function splitLines(s: string): string[] {
	if (s === '') return [];
	return (s.endsWith('\n') ? s.slice(0, -1) : s).split('\n');
}

/** Shortest line edit script from `a` to `b` (Myers), or null above `MAX_EDITS` edits. */
function myers(a: string[], b: string[]): DiffOp[] | null {
	const n = a.length;
	const m = b.length;
	const max = Math.min(n + m, MAX_EDITS);
	const off = n + m + 1;
	const v = new Int32Array(2 * off + 1);
	// trace[d] holds v for diagonals -d..d before step d.
	const trace: Int32Array[] = [];
	for (let d = 0; d <= max; d++) {
		trace.push(v.slice(off - d, off + d + 1));
		for (let k = -d; k <= d; k += 2) {
			const left = v[off + k - 1] ?? 0;
			const right = v[off + k + 1] ?? 0;
			let x = k === -d || (k !== d && left < right) ? right : left + 1;
			let y = x - k;
			while (x < n && y < m && a[x] === b[y]) {
				x++;
				y++;
			}
			v[off + k] = x;
			if (x >= n && y >= m) return backtrack(a, b, trace, d);
		}
	}
	return null;
}

function backtrack(a: string[], b: string[], trace: Int32Array[], dEnd: number): DiffOp[] {
	const ops: DiffOp[] = [];
	let x = a.length;
	let y = b.length;
	for (let d = dEnd; d > 0; d--) {
		const snap = trace[d] ?? new Int32Array(0);
		const get = (k: number) => snap[k + d] ?? 0;
		const k = x - y;
		const down = k === -d || (k !== d && get(k - 1) < get(k + 1));
		const prevK = down ? k + 1 : k - 1;
		const prevX = get(prevK);
		const midX = down ? prevX : prevX + 1;
		while (x > midX) {
			ops.push({ kind: 'context', text: a[--x] ?? '' });
			y--;
		}
		if (down) ops.push({ kind: 'added', text: b[--y] ?? '' });
		else ops.push({ kind: 'deleted', text: a[--x] ?? '' });
	}
	while (x > 0) {
		ops.push({ kind: 'context', text: a[--x] ?? '' });
		y--;
	}
	return ops.reverse();
}

/** Line edit script from `oldText` to `newText`. */
export function textLineDiff(oldText: string, newText: string): DiffOp[] {
	const a = splitLines(oldText);
	const b = splitLines(newText);
	let pre = 0;
	while (pre < a.length && pre < b.length && a[pre] === b[pre]) pre++;
	let suf = 0;
	while (suf < a.length - pre && suf < b.length - pre && a[a.length - 1 - suf] === b[b.length - 1 - suf]) suf++;
	const am = a.slice(pre, a.length - suf);
	const bm = b.slice(pre, b.length - suf);
	const mid = myers(am, bm) ?? [
		...am.map((text): DiffOp => ({ kind: 'deleted', text })),
		...bm.map((text): DiffOp => ({ kind: 'added', text }))
	];
	return [
		...a.slice(0, pre).map((text): DiffOp => ({ kind: 'context', text })),
		...mid,
		...a.slice(a.length - suf).map((text): DiffOp => ({ kind: 'context', text }))
	];
}

/**
 * A file diff of two versions of one file's text, with `context` unchanged lines around each
 * change, in the shape the diff view renders.
 */
export function textFileDiff(path: string, oldText: string, newText: string, context = 3): FileDiffNode {
	const ops = textLineDiff(oldText, newText);
	const hunks: FileDiffNode['hunks'] = [];
	let added = 0;
	let deleted = 0;
	// Old and new line numbers (1-based) before each op.
	const oldAt: number[] = [];
	const newAt: number[] = [];
	let o = 1;
	let nl = 1;
	for (const op of ops) {
		oldAt.push(o);
		newAt.push(nl);
		if (op.kind !== 'added') o++;
		if (op.kind !== 'deleted') nl++;
	}
	let i = 0;
	while (i < ops.length) {
		if (ops[i]?.kind === 'context') {
			i++;
			continue;
		}
		const start = Math.max(0, i - context);
		let end = i;
		// Extend over changes separated by at most 2 * context unchanged lines.
		for (;;) {
			while (end < ops.length && ops[end]?.kind !== 'context') end++;
			let next = end;
			while (next < ops.length && ops[next]?.kind === 'context') next++;
			if (next < ops.length && next - end <= 2 * context) end = next;
			else break;
		}
		const stop = Math.min(ops.length, end + context);
		let body = '';
		let oldLines = 0;
		let newLines = 0;
		for (const op of ops.slice(start, stop)) {
			body += `${op.kind === 'added' ? '+' : op.kind === 'deleted' ? '-' : ' '}${op.text}\n`;
			if (op.kind !== 'added') oldLines++;
			if (op.kind !== 'deleted') newLines++;
			if (op.kind === 'added') added++;
			if (op.kind === 'deleted') deleted++;
		}
		hunks.push({
			oldRange: { startLine: oldAt[start] ?? 1, lines: oldLines },
			newRange: { startLine: newAt[start] ?? 1, lines: newLines },
			body
		});
		i = stop;
	}
	return {
		oldPath: path,
		newPath: path,
		status: 'MODIFIED',
		binary: false,
		tooLarge: false,
		stat: { added, deleted },
		hunks
	};
}
