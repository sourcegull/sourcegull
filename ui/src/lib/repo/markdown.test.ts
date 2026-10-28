import { describe, expect, it } from 'vitest';
import { parseInline } from '#lib/batches/markdown.ts';
import {
	inlineText,
	isMarkdownPath,
	parseDocument,
	pickReadme,
	resolveImage,
	resolveLink,
	resolvePath,
	slugify,
	splitRow,
	type DocBlock,
	type DocLocation
} from './markdown.ts';

const kinds = (blocks: DocBlock[]) => blocks.map((b) => b.kind);

describe('parseDocument', () => {
	it('parses headings with unique ids', () => {
		const blocks = parseDocument('# Getting started #\n\nSetup\n=====\n\nMore\n---\n\n## Getting started\n\n#nope');
		expect(blocks.map((b) => (b.kind === 'heading' ? [b.level, b.id, inlineText(b.children)] : b.kind))).toEqual([
			[1, 'md-getting-started', 'Getting started'],
			[1, 'md-setup', 'Setup'],
			[2, 'md-more', 'More'],
			[2, 'md-getting-started-1', 'Getting started'],
			'para'
		]);
	});

	it('joins soft line breaks and keeps hard ones', () => {
		const [p] = parseDocument('one\ntwo  \nthree\\\nfour');
		expect(p?.kind === 'para' && inlineText(p.children)).toBe('one two\nthree\nfour');
	});

	it('parses nested lists, tasks and loose items', () => {
		const blocks = parseDocument('- a\n  - b\n  - c\n- [x] done\n- [ ] todo\n\n3. three\n4. four\n\n   para\n');
		expect(kinds(blocks)).toEqual(['list', 'list']);
		const ul = blocks[0];
		if (ul?.kind !== 'list') throw new Error('not a list');
		expect(ul.ordered).toBe(false);
		expect(ul.tight).toBe(true);
		expect(ul.items.map((i) => i.task)).toEqual([null, true, false]);
		const nested = ul.items[0]?.children[1];
		expect(nested?.kind === 'list' && nested.items.length).toBe(2);
		const ol = blocks[1];
		if (ol?.kind !== 'list') throw new Error('not a list');
		expect([ol.ordered, ol.start, ol.tight, ol.items.length]).toEqual([true, 3, false, 2]);
		expect(kinds(ol.items[1]?.children ?? [])).toEqual(['para', 'para']);
	});

	it('nests lists indented less than the item content', () => {
		const [ol] = parseDocument('1. one\n  - sub\n2. two');
		expect(ol?.kind === 'list' && ol.items.map((i) => kinds(i.children))).toEqual([['para', 'list'], ['para']]);
	});

	it('parses pipe tables with alignment', () => {
		const [t] = parseDocument('| Name | Size | Note |\n|:-----|-----:|:----:|\n| a | 1 | `x \\| y` |\n| b |\n');
		if (t?.kind !== 'table') throw new Error('not a table');
		expect(t.align).toEqual(['left', 'right', 'center']);
		expect(t.head.map(inlineText)).toEqual(['Name', 'Size', 'Note']);
		expect(t.rows.map((r) => r.map(inlineText))).toEqual([
			['a', '1', 'x | y'],
			['b', '', '']
		]);
		expect(splitRow('a | b \\| c |')).toEqual(['a', 'b | c']);
		expect(kinds(parseDocument('a | b\n--- | ---\n1 | 2'))).toEqual(['table']);
		expect(kinds(parseDocument('a | b\n--- | --- | ---'))).toEqual(['para']);
	});

	it('parses code, quotes and rules', () => {
		const blocks = parseDocument('```rust\nfn main() {}\n\n# not a heading\n```\n\n    indented\n\n> quoted\n> # title\n\n***');
		expect(kinds(blocks)).toEqual(['code', 'code', 'quote', 'rule']);
		expect(blocks[0]).toEqual({ kind: 'code', lang: 'rust', text: 'fn main() {}\n\n# not a heading' });
		expect(blocks[1]).toEqual({ kind: 'code', lang: '', text: 'indented' });
		const q = blocks[2];
		expect(q?.kind === 'quote' && kinds(q.children)).toEqual(['para', 'heading']);
	});

	it('drops raw HTML tags and comments', () => {
		const blocks = parseDocument(
			'<!-- hidden\nstill hidden -->\n<div align="center"><script>x</script>a<br/>b <!-- c --></div>\n\n<p align="center">\n<img src="x.svg" />\n</p>\n\n<https://example.com> `<b>`'
		);
		expect(blocks).toEqual([
			{ kind: 'para', children: [{ kind: 'text', text: 'xa\nb ' }] },
			{
				kind: 'para',
				children: [
					{ kind: 'link', href: 'https://example.com', children: [{ kind: 'text', text: 'https://example.com' }] },
					{ kind: 'text', text: ' ' },
					{ kind: 'code', text: '<b>' }
				]
			}
		]);
	});

	it('parses images, badges and reference links', () => {
		const [p] = parseDocument('[![build](ci.png)](https://ci.example.com) ![logo][l] [docs][] ~~old~~\n\n[l]: img/logo.png\n[Docs]: <docs/index.md> "Title"');
		expect(p?.kind === 'para' && p.children).toEqual([
			{ kind: 'link', href: 'https://ci.example.com', children: [{ kind: 'image', src: 'ci.png', alt: 'build' }] },
			{ kind: 'text', text: ' ' },
			{ kind: 'image', src: 'img/logo.png', alt: 'logo' },
			{ kind: 'text', text: ' ' },
			{ kind: 'link', href: 'docs/index.md', children: [{ kind: 'text', text: 'docs' }] },
			{ kind: 'text', text: ' ' },
			{ kind: 'del', children: [{ kind: 'text', text: 'old' }] }
		]);
	});

	it('drops unsafe image and link URLs', () => {
		const [p] = parseDocument('![x](javascript:alert(1)) [y](data:text/html,hi)');
		expect(p?.kind === 'para' && inlineText(p.children)).toBe('x) y');
	});

	it('leaves the default inline syntax unchanged', () => {
		expect(parseInline('![a](b.png) ~~c~~')).toEqual([
			{ kind: 'text', text: '!' },
			{ kind: 'link', href: 'b.png', children: [{ kind: 'text', text: 'a' }] },
			{ kind: 'text', text: ' ~~c~~' }
		]);
	});
});

describe('document URLs', () => {
	const loc: DocLocation = { repo: 'gitlab.com/g/p', rev: 'v1', path: 'a/README.md' };

	it('resolves paths', () => {
		expect(resolvePath('a/README.md', 'docs/x.md')).toBe('a/docs/x.md');
		expect(resolvePath('a/README.md', './x.md')).toBe('a/x.md');
		expect(resolvePath('a/b/README.md', '../../../x.md')).toBe('x.md');
		expect(resolvePath('a/README.md', '/x%20y.md')).toBe('x y.md');
	});

	it('resolves links to the same repository and revision', () => {
		expect(resolveLink('docs/x.md', loc)).toBe('/gitlab.com/g/p@v1/-/blob/a/docs/x.md');
		expect(resolveLink('../src/', loc)).toBe('/gitlab.com/g/p@v1/-/tree/src');
		expect(resolveLink('/LICENSE', loc)).toBe('/gitlab.com/g/p@v1/-/blob/LICENSE');
		expect(resolveLink('..', loc)).toBe('/gitlab.com/g/p@v1');
		expect(resolveLink('x.md?plain=1#Usage', loc)).toBe('/gitlab.com/g/p@v1/-/blob/a/x.md#md-usage');
		expect(resolveLink('main.rs#L10-20', loc)).toBe('/gitlab.com/g/p@v1/-/blob/a/main.rs#L10-20');
		expect(resolveLink('#install', loc)).toBe('#md-install');
		expect(resolveLink('#L5', loc)).toBe('?view=code#L5');
		expect(resolveLink('docs/x.md', { ...loc, rev: null })).toBe('/gitlab.com/g/p/-/blob/a/docs/x.md');
	});

	it('keeps absolute links and drops unsafe ones', () => {
		expect(resolveLink('https://example.com/a', loc)).toBe('https://example.com/a');
		expect(resolveLink('mailto:a@example.com', loc)).toBe('mailto:a@example.com');
		expect(resolveLink('javascript:alert(1)', loc)).toBeNull();
	});

	it('loads relative images through the raw endpoint', () => {
		expect(resolveImage('img/logo.png', loc)).toEqual({ kind: 'image', src: '/gitlab.com/g/p@v1/-/raw/a/img/logo.png' });
		expect(resolveImage('/docs/shot.JPG?raw=true', loc)).toEqual({ kind: 'image', src: '/gitlab.com/g/p@v1/-/raw/docs/shot.JPG' });
		expect(resolveImage('diagram.svg', loc)).toEqual({ kind: 'link', href: '/gitlab.com/g/p@v1/-/blob/a/diagram.svg' });
		expect(resolveImage('https://img.example.com/b.png', loc)).toEqual({ kind: 'link', href: 'https://img.example.com/b.png' });
		expect(resolveImage('//img.example.com/b.png', loc)).toEqual({ kind: 'link', href: '//img.example.com/b.png' });
		expect(resolveImage('data:image/png;base64,AAAA', loc)).toEqual({ kind: 'text' });
		expect(resolveImage('mailto:a@example.com', loc)).toEqual({ kind: 'text' });
	});
});

describe('README files', () => {
	const e = (name: string, isDirectory = false, kind = 'FILE') => ({ name, isDirectory, kind });

	it('picks the README, preferring Markdown', () => {
		expect(pickReadme([e('README'), e('readme.markdown'), e('src', true)])?.name).toBe('readme.markdown');
		expect(pickReadme([e('README'), e('Readme.MD')])?.name).toBe('Readme.MD');
		expect(pickReadme([e('README')])?.name).toBe('README');
		expect(pickReadme([e('README.md', true), e('README.md', false, 'SYMLINK'), e('README.txt')])).toBeNull();
	});

	it('detects Markdown files and slugs', () => {
		expect(isMarkdownPath('docs/A.MD')).toBe(true);
		expect(isMarkdownPath('x.markdown')).toBe(true);
		expect(isMarkdownPath('x.mdx')).toBe(false);
		expect(slugify('Hello, World! (v2) — café')).toBe('hello-world-v2--café');
	});
});
