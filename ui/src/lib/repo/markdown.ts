/**
 * Markdown documents in repositories (rendered `.md` files and directory READMEs).
 *
 * `parseDocument` builds on the inline parser of `#lib/batches/markdown.ts` and adds nested
 * lists, task items, GFM pipe tables, setext headings, indented code, reference links and
 * images. The result is plain data that `MarkdownView.svelte` renders with text nodes only:
 * raw HTML tags and comments in the source are dropped (the text between tags stays), and
 * URLs other than http(s), mailto and relative ones are dropped.
 *
 * `resolveLink` and `resolveImage` map the URLs of a document to the same repository and
 * revision: relative paths go to file and directory pages, images load through the raw file
 * endpoint (so the viewer's repository permissions apply), and heading anchors get the
 * `md-` prefix of heading ids.
 */

import { parseInline, refKey, safeHref, type Inline } from '#lib/batches/markdown.ts';
import { parentPath, repoPageURL } from './url.ts';
import type { ReadmeBlobQuery } from '#lib/graphql-types.ts';

export type { Inline } from '#lib/batches/markdown.ts';

export type Align = 'left' | 'center' | 'right' | null;

export interface ListItem {
	/** `[ ]` (false) or `[x]` (true) task item; null for a plain item. */
	task: boolean | null;
	children: DocBlock[];
}

export type DocBlock =
	| { kind: 'heading'; level: number; id: string; children: Inline[] }
	| { kind: 'para'; children: Inline[] }
	| { kind: 'code'; lang: string; text: string }
	| { kind: 'rule' }
	| { kind: 'quote'; children: DocBlock[] }
	| { kind: 'list'; ordered: boolean; start: number; tight: boolean; items: ListItem[] }
	| { kind: 'table'; align: Align[]; head: Inline[][]; rows: Inline[][][] };

interface Ctx {
	refs: Map<string, string>;
	slugs: Map<string, number>;
}

const FENCE = /^ {0,3}(`{3,}|~{3,})\s*([^\s`]*)/;
const ATX = /^ {0,3}(#{1,6})(?=\s|$)(.*)$/;
const SETEXT = /^ {0,3}(=+|-+)\s*$/;
const RULE = /^ {0,3}([-*_])(?:[ \t]*\1){2,}[ \t]*$/;
const QUOTE = /^ {0,3}> ?(.*)$/;
const ITEM = /^( {0,3})([-*+]|(\d{1,9})[.)])(?:([ \t]+)(.*))?$/;
const COMMENT = /^ {0,3}<!--/;
const DELIMITER = /^ {0,3}\|?\s*:?-+:?\s*(\|\s*:?-+:?\s*)*\|?\s*$/;
const REF_DEF = /^ {0,3}\[([^\]\n]+)\]:\s*(<[^<>\n]*>|\S+)(?:\s+(?:"[^"]*"|'[^']*'|\([^)]*\)))?\s*$/;

/** Prefix of heading ids, so they cannot clash with ids of the page. */
export const ANCHOR_PREFIX = 'md-';

const isBlank = (line: string) => line.trim() === '';
const indentOf = (line: string) => /^ */.exec(line)?.[0].length ?? 0;

function expandTabs(line: string): string {
	if (!line.includes('\t')) return line;
	let out = '';
	for (const ch of line) out += ch === '\t' ? ' '.repeat(4 - (out.length % 4)) : ch;
	return out;
}

interface ItemStart {
	indent: number;
	bullet: string;
	ordered: boolean;
	start: number;
	contentIndent: number;
	content: string;
}

function itemStart(line: string): ItemStart | null {
	const m = ITEM.exec(line);
	if (!m) return null;
	const indent = m[1]?.length ?? 0;
	const marker = m[2] ?? '';
	const spaces = m[4]?.length ?? 0;
	const content = m[5] ?? '';
	const gap = spaces === 0 || spaces > 4 || content === '' ? 1 : spaces;
	return {
		indent,
		bullet: m[3] === undefined ? marker : marker.slice(-1),
		ordered: m[3] !== undefined,
		start: m[3] === undefined ? 1 : Number(m[3]),
		contentIndent: indent + marker.length + gap,
		content: spaces > 4 ? ' '.repeat(spaces - 1) + content : content
	};
}

/** Whether a line starts a block that ends a paragraph. */
function interrupts(line: string): boolean {
	if (isBlank(line) || FENCE.test(line) || ATX.test(line) || RULE.test(line) || QUOTE.test(line)) return true;
	if (COMMENT.test(line)) return true;
	const item = itemStart(line);
	return item !== null && item.content.trim() !== '' && (!item.ordered || item.start === 1);
}

/** Splits a table row into cells on unescaped pipes; `\|` becomes `|` (also in code spans). */
export function splitRow(line: string): string[] {
	let s = line.trim();
	if (s.startsWith('|')) s = s.slice(1);
	if (s.endsWith('|') && !s.endsWith('\\|')) s = s.slice(0, -1);
	const cells: string[] = [];
	let cur = '';
	for (let i = 0; i < s.length; i++) {
		const ch = s[i];
		if (ch === '\\' && s[i + 1] === '|') {
			cur += '|';
			i++;
		} else if (ch === '|') {
			cells.push(cur.trim());
			cur = '';
		} else {
			cur += ch;
		}
	}
	cells.push(cur.trim());
	return cells;
}

function alignOf(cell: string): Align {
	const left = cell.startsWith(':');
	const right = cell.endsWith(':');
	return left && right ? 'center' : right ? 'right' : left ? 'left' : null;
}

/** Joins paragraph lines: soft breaks become spaces, hard breaks (two spaces or `\`) newlines. */
function joinLines(lines: string[]): string {
	let out = '';
	lines.forEach((raw, i) => {
		const line = raw.replace(/^\s+/, '');
		if (i === lines.length - 1) {
			out += line.trimEnd();
		} else if (/ {2,}$/.test(line)) {
			out += `${line.trimEnd()}\n`;
		} else if (/\\$/.test(line)) {
			out += `${line.slice(0, -1)}\n`;
		} else {
			out += `${line.trimEnd()} `;
		}
	});
	return out;
}

/** Plain text of inline nodes. */
export function inlineText(nodes: Inline[]): string {
	return nodes
		.map((n) => {
			switch (n.kind) {
				case 'text':
				case 'code':
					return n.text;
				case 'image':
					return n.alt;
				default:
					return inlineText(n.children);
			}
		})
		.join('');
}

/** A heading's anchor slug: lower case letters, digits, `-` and `_`, spaces as `-`. */
export function slugify(text: string): string {
	return text
		.trim()
		.toLowerCase()
		.replace(/[^\p{L}\p{N}\s_-]/gu, '')
		.replace(/\s/g, '-');
}

function headingID(ctx: Ctx, children: Inline[]): string {
	const slug = slugify(inlineText(children));
	const n = ctx.slugs.get(slug) ?? 0;
	ctx.slugs.set(slug, n + 1);
	return ANCHOR_PREFIX + (n === 0 ? slug : `${slug}-${n}`);
}

function inline(ctx: Ctx, text: string): Inline[] {
	return parseInline(text, { rich: true, refs: ctx.refs });
}

function parseBlocks(lines: string[], ctx: Ctx): DocBlock[] {
	const out: DocBlock[] = [];
	let para: string[] = [];
	const flush = () => {
		const children = para.length > 0 ? inline(ctx, joinLines(para)) : [];
		// A paragraph of HTML tags only is empty once they are dropped.
		if (!children.every((n) => n.kind === 'text' && n.text.trim() === '')) out.push({ kind: 'para', children });
		para = [];
	};
	const heading = (level: number, text: string) => {
		const children = inline(ctx, text);
		out.push({ kind: 'heading', level, id: headingID(ctx, children), children });
	};

	let i = 0;
	while (i < lines.length) {
		const line = lines[i] ?? '';
		if (isBlank(line)) {
			flush();
			i++;
			continue;
		}
		if (para.length === 0 && indentOf(line) >= 4) {
			const body: string[] = [];
			while (i < lines.length && (isBlank(lines[i] ?? '') || indentOf(lines[i] ?? '') >= 4)) {
				body.push((lines[i] ?? '').slice(4));
				i++;
			}
			while (body.length > 0 && isBlank(body[body.length - 1] ?? '')) body.pop();
			out.push({ kind: 'code', lang: '', text: body.join('\n') });
			continue;
		}
		const fence = FENCE.exec(line);
		const marker = fence?.[1] ?? '';
		// The info string of a backtick fence has no backticks.
		if (fence && !(marker.startsWith('`') && line.trim().slice(marker.length).includes('`'))) {
			flush();
			const strip = indentOf(line);
			const close = new RegExp(`^ {0,3}${marker[0] === '`' ? '`' : '~'}{${marker.length},}\\s*$`);
			const body: string[] = [];
			i++;
			while (i < lines.length) {
				const l = lines[i] ?? '';
				if (close.test(l)) break;
				body.push(l.slice(Math.min(strip, indentOf(l))));
				i++;
			}
			i++;
			out.push({ kind: 'code', lang: fence[2] ?? '', text: body.join('\n') });
			continue;
		}
		if (COMMENT.test(line)) {
			flush();
			while (i < lines.length && !(lines[i] ?? '').includes('-->')) i++;
			i++;
			continue;
		}
		const atx = ATX.exec(line);
		if (atx) {
			flush();
			const text = (atx[2] ?? '').replace(/(^|\s+)#+\s*$/, '').trim();
			heading(atx[1]?.length ?? 1, text);
			i++;
			continue;
		}
		const setext = para.length > 0 ? SETEXT.exec(line) : null;
		if (setext) {
			const text = joinLines(para);
			para = [];
			heading(setext[1]?.startsWith('=') ? 1 : 2, text);
			i++;
			continue;
		}
		if (RULE.test(line)) {
			flush();
			out.push({ kind: 'rule' });
			i++;
			continue;
		}
		const next = lines[i + 1] ?? '';
		if (line.includes('|') && DELIMITER.test(next)) {
			const head = splitRow(line);
			const delims = splitRow(next);
			if (head.length === delims.length) {
				flush();
				const width = head.length;
				const fit = (cells: string[]) =>
					Array.from({ length: width }, (_, k) => inline(ctx, cells[k] ?? ''));
				const rows: Inline[][][] = [];
				i += 2;
				while (i < lines.length && !interrupts(lines[i] ?? '')) {
					rows.push(fit(splitRow(lines[i] ?? '')));
					i++;
				}
				out.push({ kind: 'table', align: delims.map(alignOf), head: fit(head), rows });
				continue;
			}
		}
		if (QUOTE.test(line)) {
			flush();
			const body: string[] = [];
			while (i < lines.length) {
				const l = lines[i] ?? '';
				const q = QUOTE.exec(l);
				if (q) body.push(q[1] ?? '');
				else if (!interrupts(l) && !isBlank(body[body.length - 1] ?? '')) body.push(l);
				else break;
				i++;
			}
			out.push({ kind: 'quote', children: parseBlocks(body, ctx) });
			continue;
		}
		const item = itemStart(line);
		if (item && (para.length === 0 || interrupts(line))) {
			flush();
			i = parseList(lines, i, item, ctx, out);
			continue;
		}
		para.push(line);
		i++;
	}
	flush();
	return out;
}

/** Parses the list starting at `lines[i]` into `out`; returns the index after it. */
function parseList(lines: string[], i: number, first: ItemStart, ctx: Ctx, out: DocBlock[]): number {
	const items: ListItem[] = [];
	let tight = true;
	let cur: ItemStart | null = first;
	while (cur) {
		const body = [cur.content];
		const contentIndent = cur.contentIndent;
		i++;
		let nextItem: ItemStart | null = null;
		while (i < lines.length) {
			const l = lines[i] ?? '';
			if (isBlank(l)) {
				body.push('');
				i++;
				continue;
			}
			const ind = indentOf(l);
			if (ind >= contentIndent) {
				body.push(l.slice(contentIndent));
				i++;
				continue;
			}
			const st = itemStart(l);
			if (st && st.ordered === first.ordered && st.bullet === first.bullet && st.indent <= first.indent + 1) {
				nextItem = st;
				break;
			}
			if (st && ind > first.indent) {
				// A nested list indented less than the content: lenient nesting.
				body.push(l.slice(ind));
				i++;
				continue;
			}
			if (!isBlank(body[body.length - 1] ?? '') && !interrupts(l) && !st) {
				body.push(l.trim());
				i++;
				continue;
			}
			break;
		}
		let trailing = 0;
		while (body.length > 1 && isBlank(body[body.length - 1] ?? '')) {
			body.pop();
			trailing++;
		}
		if (trailing > 0 && nextItem) tight = false;
		if (hasInnerBlank(body)) tight = false;
		let task: boolean | null = null;
		const t = /^\[([ xX])\]\s+/.exec(body[0] ?? '');
		if (t) {
			task = t[1] !== ' ';
			body[0] = (body[0] ?? '').slice(t[0].length);
		}
		items.push({ task, children: parseBlocks(body, ctx) });
		cur = nextItem;
	}
	out.push({ kind: 'list', ordered: first.ordered, start: first.start, tight, items });
	return i;
}

/** Whether a blank line separates two blocks of an item (outside fenced code). */
function hasInnerBlank(body: string[]): boolean {
	let fence: string | null = null;
	for (let k = 0; k < body.length; k++) {
		const l = body[k] ?? '';
		const f = /^ {0,3}(`{3,}|~{3,})/.exec(l);
		if (f) fence = fence === null ? (f[1] ?? null) : null;
		if (fence === null && isBlank(l) && k > 0 && k < body.length - 1) {
			// A blank line inside a nested list does not loosen this list.
			const nextLine = body.slice(k + 1).find((x) => !isBlank(x)) ?? '';
			if (indentOf(nextLine) === 0) return true;
		}
	}
	return false;
}

/** Collects reference link definitions outside fenced code and removes their lines. */
function takeDefinitions(lines: string[], refs: Map<string, string>): string[] {
	const out: string[] = [];
	let fence: string | null = null;
	// Definitions cannot interrupt a paragraph.
	let canStart = true;
	for (const line of lines) {
		const f = /^\s*(`{3,}|~{3,})/.exec(line);
		if (f) fence = fence === null ? (f[1] ?? null) : null;
		const m = fence === null && canStart ? REF_DEF.exec(line) : null;
		if (m) {
			const key = refKey(m[1] ?? '');
			const raw = m[2] ?? '';
			if (!refs.has(key)) refs.set(key, raw.startsWith('<') ? raw.slice(1, -1) : raw);
			continue;
		}
		canStart = isBlank(line) || ATX.test(line);
		out.push(line);
	}
	return out;
}

/** Parses a Markdown document into blocks. */
export function parseDocument(md: string): DocBlock[] {
	const ctx: Ctx = { refs: new Map(), slugs: new Map() };
	const lines = md.replace(/^﻿/, '').replace(/\r\n?/g, '\n').split('\n').map(expandTabs);
	return parseBlocks(takeDefinitions(lines, ctx.refs), ctx);
}

// ---- URLs -------------------------------------------------------------------------------------

/** Where a document lives: its links resolve against this. */
export interface DocLocation {
	repo: string;
	/** Revision as in the page URL; null for the default branch. */
	rev: string | null;
	/** Path of the Markdown file. */
	path: string;
}

const EXTERNAL = /^(?:[a-z][a-z0-9+.-]*:|\/\/)/i;

function decodeSegment(s: string): string {
	try {
		return decodeURIComponent(s);
	} catch {
		return s;
	}
}

/** Resolves a relative path against the document's directory (or the root for `/…`). */
export function resolvePath(docPath: string, target: string): string {
	const base = target.startsWith('/') ? [] : parentPath(docPath).split('/').filter(Boolean);
	for (const seg of target.split('/')) {
		if (seg === '' || seg === '.') continue;
		if (seg === '..') base.pop();
		else base.push(decodeSegment(seg));
	}
	return base.join('/');
}

/** The id fragment for a link anchor: line ranges stay, other anchors get the heading prefix. */
function fragment(hash: string): string {
	if (hash === '') return '';
	if (/^L\d+/.test(hash)) return `#${hash}`;
	const id = decodeSegment(hash);
	return `#${id.startsWith(ANCHOR_PREFIX) ? id : ANCHOR_PREFIX + id.toLowerCase()}`;
}

function splitTarget(href: string): { path: string; hash: string } {
	const h = href.indexOf('#');
	const hash = h >= 0 ? href.slice(h + 1) : '';
	let path = h >= 0 ? href.slice(0, h) : href;
	const q = path.indexOf('?');
	if (q >= 0) path = path.slice(0, q);
	return { path, hash };
}

/** The URL a document link points to; null if it is not allowed. */
export function resolveLink(href: string, loc: DocLocation): string | null {
	const safe = safeHref(href);
	if (safe === null) return null;
	if (EXTERNAL.test(safe)) return safe;
	const { path, hash } = splitTarget(safe);
	if (path === '') {
		// The same document: a line range shows the code, other anchors are headings.
		if (/^L\d+/.test(hash)) return `?view=code#${hash}`;
		return fragment(hash) || '#';
	}
	const target = resolvePath(loc.path, path);
	if (target === '') return repoPageURL(loc.repo, loc.rev, 'root') + fragment(hash);
	const dir = path.endsWith('/');
	return repoPageURL(loc.repo, loc.rev, dir ? 'tree' : 'blob', target) + fragment(hash);
}

/** Image types the raw file endpoint serves inline. */
const INLINE_IMAGE = /\.(png|jpe?g|gif|webp|bmp|ico|avif)$/i;

export type ResolvedImage =
	| { kind: 'image'; src: string }
	| { kind: 'link'; href: string }
	| { kind: 'text' };

/**
 * How to show an image: relative images in formats the raw endpoint serves inline load from
 * it; other relative files and external images (the content security policy allows images
 * from Sourcegull only) become links labeled with the alt text.
 */
export function resolveImage(src: string, loc: DocLocation): ResolvedImage {
	const safe = safeHref(src);
	if (safe === null) return { kind: 'text' };
	if (/^https?:|^\/\//i.test(safe)) return { kind: 'link', href: safe };
	if (EXTERNAL.test(safe)) return { kind: 'text' };
	const { path } = splitTarget(safe);
	const target = resolvePath(loc.path, path);
	if (target === '') return { kind: 'text' };
	if (INLINE_IMAGE.test(target)) return { kind: 'image', src: repoPageURL(loc.repo, loc.rev, 'raw', target) };
	return { kind: 'link', href: repoPageURL(loc.repo, loc.rev, 'blob', target) };
}

// ---- Files ------------------------------------------------------------------------------------

/** Whether a file is shown rendered as Markdown. */
export function isMarkdownPath(path: string): boolean {
	return /\.(md|markdown)$/i.test(path);
}

/** The README of a directory listing: README.md, then README.markdown, then README (any case). */
export function pickReadme<T extends { name: string; isDirectory: boolean; kind?: string }>(
	entries: readonly T[]
): T | null {
	const rank = (name: string) => {
		const n = name.toLowerCase();
		return n === 'readme.md' ? 0 : n === 'readme.markdown' ? 1 : n === 'readme' ? 2 : -1;
	};
	let best: T | null = null;
	for (const e of entries) {
		if (e.isDirectory || e.kind === 'SYMLINK' || e.kind === 'SUBMODULE') continue;
		const r = rank(e.name);
		if (r >= 0 && (best === null || r < rank(best.name))) best = e;
	}
	return best;
}

export const README_BLOB = `query ReadmeBlob($repo: String!, $rev: String!, $path: String!) {
	repository(name: $repo) {
		commit(rev: $rev) {
			blob(path: $path) { path name url binary contentTruncated content }
		}
	}
}`;

export interface ReadmeBlob {
	path: string;
	name: string;
	url: string;
	binary: boolean;
	contentTruncated: boolean;
	content: string;
}

export type ReadmeBlobResult = ReadmeBlobQuery;
