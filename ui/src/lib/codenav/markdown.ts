/**
 * A small Markdown renderer for hover text: fenced code blocks, `---` rules, paragraphs and
 * inline code, with backslash escapes and character references resolved. It builds DOM nodes and never interprets HTML, so hover text from uploads and
 * repositories cannot inject markup.
 */

export type Block =
	| { kind: 'code'; lang: string; text: string }
	| { kind: 'rule' }
	| { kind: 'para'; text: string };

/** Splits Markdown into blocks. */
export function parseBlocks(md: string): Block[] {
	const lines = md.replace(/\r\n/g, '\n').split('\n');
	const out: Block[] = [];
	let para: string[] = [];
	const flush = (): void => {
		if (para.length > 0) out.push({ kind: 'para', text: para.join('\n') });
		para = [];
	};
	for (let i = 0; i < lines.length; i++) {
		const line = lines[i] ?? '';
		const fence = /^(`{3,}|~{3,})\s*([\w+#.-]*)\s*$/.exec(line);
		if (fence) {
			flush();
			const marker = fence[1] ?? '```';
			const body: string[] = [];
			i++;
			while (i < lines.length && !(lines[i] ?? '').startsWith(marker)) body.push(lines[i++] ?? '');
			out.push({ kind: 'code', lang: fence[2] ?? '', text: body.join('\n') });
			continue;
		}
		if (/^\s*(-{3,}|\*{3,}|_{3,})\s*$/.test(line)) {
			flush();
			out.push({ kind: 'rule' });
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

/** Named character references hover text uses (scip-python writes `&nbsp;` for indentation). */
const ENTITIES: Record<string, string> = {
	nbsp: '\u00a0',
	amp: '&',
	lt: '<',
	gt: '>',
	quot: '"',
	apos: "'",
	hellip: '\u2026',
	ndash: '\u2013',
	mdash: '\u2014'
};

/**
 * Resolves backslash escapes of ASCII punctuation (`secret\_key`) and character references
 * (`&nbsp;`, `&#39;`, `&#x27;`) in Markdown text outside code, as CommonMark does. Unknown
 * references stay as they are.
 */
export function unescapeText(text: string): string {
	return text.replace(
		/\\([!-/:-@[-`{-~])|&(#[0-9]{1,7}|#[xX][0-9a-fA-F]{1,6}|[A-Za-z][A-Za-z0-9]{1,31});/g,
		(whole, escaped: string | undefined, ref: string | undefined) => {
			if (escaped !== undefined) return escaped;
			if (ref === undefined) return whole;
			if (ref.startsWith('#')) {
				const code = ref[1] === 'x' || ref[1] === 'X' ? parseInt(ref.slice(2), 16) : parseInt(ref.slice(1), 10);
				return code > 0 && code <= 0x10ffff && !(code >= 0xd800 && code <= 0xdfff)
					? String.fromCodePoint(code)
					: '\ufffd';
			}
			return ENTITIES[ref] ?? whole;
		}
	);
}

/** A paragraph's text as `inline code` spans and plain text (escapes resolved). */
export function inlineSpans(text: string): { code: boolean; text: string }[] {
	const out: { code: boolean; text: string }[] = [];
	for (const part of text.split(/(`[^`\n]+`)/)) {
		if (part.length > 2 && part.startsWith('`') && part.endsWith('`')) {
			out.push({ code: true, text: part.slice(1, -1) });
		} else if (part) {
			out.push({ code: false, text: unescapeText(part) });
		}
	}
	return out;
}

/** Appends `text` to `parent`, with `inline code` spans. */
function appendInline(parent: HTMLElement, text: string): void {
	for (const span of inlineSpans(text)) {
		if (span.code) {
			const code = document.createElement('code');
			code.textContent = span.text;
			parent.append(code);
		} else {
			parent.append(document.createTextNode(span.text));
		}
	}
}

/** Renders Markdown into a fragment. */
export function renderMarkdown(md: string): DocumentFragment {
	const frag = document.createDocumentFragment();
	for (const b of parseBlocks(md)) {
		if (b.kind === 'code') {
			const pre = document.createElement('pre');
			const code = document.createElement('code');
			if (b.lang) code.dataset['lang'] = b.lang;
			code.textContent = b.text;
			pre.append(code);
			frag.append(pre);
		} else if (b.kind === 'rule') {
			frag.append(document.createElement('hr'));
		} else {
			const p = document.createElement('p');
			appendInline(p, b.text);
			frag.append(p);
		}
	}
	return frag;
}
