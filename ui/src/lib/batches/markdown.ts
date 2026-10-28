/**
 * A small Markdown parser for bulk change descriptions: headings, lists, fenced code, rules,
 * quotes and paragraphs with `code`, **strong**, *emphasis* and [links](https://…). It returns
 * plain data that components render with text nodes, so descriptions cannot inject markup;
 * links are kept only for http(s), mailto and relative URLs. `parseInline` also takes
 * options for documents (`#lib/repo/markdown.ts`): images, ~~strikethrough~~, `<url>`
 * autolinks, dropped HTML tags and reference links.
 */

export type Inline =
	| { kind: 'text'; text: string }
	| { kind: 'code'; text: string }
	| { kind: 'strong'; children: Inline[] }
	| { kind: 'em'; children: Inline[] }
	| { kind: 'link'; href: string; children: Inline[] }
	| { kind: 'del'; children: Inline[] }
	| { kind: 'image'; src: string; alt: string };

/** Inline syntax beyond the defaults; without options only the kinds above `del` appear. */
export interface InlineOptions {
	/**
	 * Images, ~~strikethrough~~ and `<https://…>` autolinks; HTML tags are dropped (the text
	 * between them stays) and `<br>` is a line break.
	 */
	rich?: boolean;
	/** Reference link definitions (`[label]: url`), keyed by `refKey(label)`. */
	refs?: ReadonlyMap<string, string>;
}

/** The lookup key of a reference link label. */
export function refKey(label: string): string {
	return label.trim().replace(/\s+/g, ' ').toLowerCase();
}

const LABEL = String.raw`\[((?:[^\[\]\n\\]|\\.|\[[^\[\]\n]*\])+)\]`;
const DEST = String.raw`\(\s*(<[^<>\n]+>|[^)\s]+)(?:\s+(?:"[^"\n]*"|'[^'\n]*'))?\s*\)`;
const LINK = new RegExp(`^${LABEL}${DEST}`);
const IMAGE = new RegExp(String.raw`^!\[([^\]\n]*)\]` + DEST);
const REF_LINK = new RegExp(`^${LABEL}(?:\\[([^\\]\\n]*)\\])?`);
const REF_IMAGE = /^!\[([^\]\n]*)\](?:\[([^\]\n]*)\])?/;
const HTML_TAG = /^(?:<\/?[a-z][a-z0-9-]*(?:\s+[^<>]*?)?\/?>|<!--[\s\S]*?-->)/i;

function destination(raw: string): string {
	return raw.startsWith('<') && raw.endsWith('>') ? raw.slice(1, -1) : raw;
}

export type MdBlock =
	| { kind: 'heading'; level: number; children: Inline[] }
	| { kind: 'para'; children: Inline[] }
	| { kind: 'code'; text: string }
	| { kind: 'rule' }
	| { kind: 'quote'; children: Inline[] }
	| { kind: 'list'; ordered: boolean; items: Inline[][] };

/** Whether a link target is safe to render. */
export function safeHref(href: string): string | null {
	const h = href.trim();
	if (/^(https?:|mailto:)/i.test(h)) return h;
	if (/^[a-z][a-z0-9+.-]*:/i.test(h)) return null;
	return h === '' ? null : h;
}

/** Parses inline Markdown. */
export function parseInline(text: string, opts: InlineOptions = {}): Inline[] {
	const refs = opts.refs;
	const out: Inline[] = [];
	let buf = '';
	const flush = () => {
		if (buf) out.push({ kind: 'text', text: buf });
		buf = '';
	};
	let i = 0;
	while (i < text.length) {
		const rest = text.slice(i);
		const ch = text[i];
		if (ch === '\\' && i + 1 < text.length && /[!-/:-@[-`{-~]/.test(text[i + 1] ?? '')) {
			buf += text[i + 1];
			i += 2;
			continue;
		}
		let m: RegExpExecArray | null;
		if (ch === '`' && (m = /^`([^`\n]+)`/.exec(rest))) {
			flush();
			out.push({ kind: 'code', text: m[1] ?? '' });
			i += m[0].length;
			continue;
		}
		if ((ch === '*' || ch === '_') && (m = /^(\*\*|__)(?=\S)([\s\S]+?)(?<=\S)\1/.exec(rest))) {
			flush();
			out.push({ kind: 'strong', children: parseInline(m[2] ?? '', opts) });
			i += m[0].length;
			continue;
		}
		if ((ch === '*' || ch === '_') && (m = /^([*_])(?=\S)([^*_\n]+?)(?<=\S)\1/.exec(rest))) {
			flush();
			out.push({ kind: 'em', children: parseInline(m[2] ?? '', opts) });
			i += m[0].length;
			continue;
		}
		if (opts.rich && ch === '~' && (m = /^~~(?=\S)([\s\S]+?)(?<=\S)~~/.exec(rest))) {
			flush();
			out.push({ kind: 'del', children: parseInline(m[1] ?? '', opts) });
			i += m[0].length;
			continue;
		}
		if (opts.rich && ch === '<' && (m = /^<(https?:\/\/[^\s<>]+)>/i.exec(rest))) {
			flush();
			out.push({ kind: 'link', href: m[1] ?? '', children: [{ kind: 'text', text: m[1] ?? '' }] });
			i += m[0].length;
			continue;
		}
		if (opts.rich && ch === '<' && (m = HTML_TAG.exec(rest))) {
			if (/^<br\b/i.test(m[0])) buf += '\n';
			i += m[0].length;
			continue;
		}
		if (opts.rich && ch === '!' && (m = IMAGE.exec(rest))) {
			const src = safeHref(destination(m[2] ?? ''));
			flush();
			if (src) out.push({ kind: 'image', src, alt: m[1] ?? '' });
			else if (m[1]) out.push({ kind: 'text', text: m[1] });
			i += m[0].length;
			continue;
		}
		if (opts.rich && refs && ch === '!' && (m = REF_IMAGE.exec(rest))) {
			const url = refs.get(refKey(m[2] || m[1] || ''));
			const src = url === undefined ? null : safeHref(url);
			if (src) {
				flush();
				out.push({ kind: 'image', src, alt: m[1] ?? '' });
				i += m[0].length;
				continue;
			}
		}
		if (ch === '[' && (m = LINK.exec(rest))) {
			const href = safeHref(destination(m[2] ?? ''));
			flush();
			if (href) out.push({ kind: 'link', href, children: parseInline(m[1] ?? '', opts) });
			else out.push(...parseInline(m[1] ?? '', opts));
			i += m[0].length;
			continue;
		}
		if (refs && ch === '[' && (m = REF_LINK.exec(rest))) {
			const url = refs.get(refKey(m[2] || m[1] || ''));
			const href = url === undefined ? null : safeHref(url);
			if (href) {
				flush();
				out.push({ kind: 'link', href, children: parseInline(m[1] ?? '', opts) });
				i += m[0].length;
				continue;
			}
		}
		if ((ch === 'h') && (m = /^https?:\/\/[^\s<>()]+[^\s<>().,;:!?'"]/.exec(rest)) && !/[\w/]$/.test(buf)) {
			flush();
			out.push({ kind: 'link', href: m[0], children: [{ kind: 'text', text: m[0] }] });
			i += m[0].length;
			continue;
		}
		buf += ch;
		i++;
	}
	flush();
	return out;
}

/** Parses Markdown into blocks. */
export function parseMarkdown(md: string): MdBlock[] {
	const lines = md.replace(/\r\n?/g, '\n').split('\n');
	const out: MdBlock[] = [];
	let para: string[] = [];
	const flush = () => {
		if (para.length > 0) out.push({ kind: 'para', children: parseInline(para.join('\n')) });
		para = [];
	};
	for (let i = 0; i < lines.length; i++) {
		const line = lines[i] ?? '';
		const fence = /^\s*(`{3,}|~{3,})/.exec(line);
		if (fence) {
			flush();
			const marker = fence[1] ?? '```';
			const body: string[] = [];
			i++;
			while (i < lines.length && !(lines[i] ?? '').trimStart().startsWith(marker)) body.push(lines[i++] ?? '');
			out.push({ kind: 'code', text: body.join('\n') });
			continue;
		}
		const heading = /^(#{1,6})\s+(.*?)\s*#*\s*$/.exec(line);
		if (heading) {
			flush();
			out.push({ kind: 'heading', level: heading[1]?.length ?? 1, children: parseInline(heading[2] ?? '') });
			continue;
		}
		if (/^\s{0,3}([-*_])(\s*\1){2,}\s*$/.test(line)) {
			flush();
			out.push({ kind: 'rule' });
			continue;
		}
		const item = /^\s{0,3}([-*+]|\d{1,9}[.)])\s+(.*)$/.exec(line);
		if (item) {
			flush();
			const ordered = /\d/.test(item[1] ?? '');
			const items: Inline[][] = [];
			let current = [item[2] ?? ''];
			while (++i < lines.length) {
				const next = lines[i] ?? '';
				const m = /^\s{0,3}([-*+]|\d{1,9}[.)])\s+(.*)$/.exec(next);
				if (m && /\d/.test(m[1] ?? '') === ordered) {
					items.push(parseInline(current.join('\n')));
					current = [m[2] ?? ''];
				} else if (/^\s+\S/.test(next)) {
					current.push(next.trim());
				} else {
					break;
				}
			}
			items.push(parseInline(current.join('\n')));
			i--;
			out.push({ kind: 'list', ordered, items });
			continue;
		}
		const quote = /^\s{0,3}>\s?(.*)$/.exec(line);
		if (quote) {
			flush();
			const body = [quote[1] ?? ''];
			while (i + 1 < lines.length && /^\s{0,3}>/.test(lines[i + 1] ?? '')) {
				body.push((lines[++i] ?? '').replace(/^\s{0,3}>\s?/, ''));
			}
			out.push({ kind: 'quote', children: parseInline(body.join('\n')) });
			continue;
		}
		if (line.trim() === '') {
			flush();
			continue;
		}
		para.push(line);
	}
	flush();
	return out;
}
