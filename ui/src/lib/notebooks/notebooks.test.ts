import { describe, expect, it } from 'vitest';
import { blockToInput, inputToBlock } from './queries.ts';
import { pickSymbol, symbolSearchQuery, symbolURLRange } from './symbols.ts';
import {
	blockFromURL,
	markdownToBlocks,
	notebookFileName,
	parseBlockLineRange,
	serializeBlock,
	serializeLineRange,
	serializeNotebook,
	titleFromFileName,
	type Block
} from './serialize.ts';

const ORIGIN = 'https://sourcegull.example.com';
const REPO = 'gitlab.com/acme/web';

describe('.snb.md serialization', () => {
	it('serializes Markdown and query blocks', () => {
		expect(serializeBlock({ type: 'md', input: { text: '' } }, ORIGIN)).toBe('');
		expect(serializeBlock({ type: 'md', input: { text: '# Title\n\n' } }, ORIGIN)).toBe('# Title');
		expect(serializeBlock({ type: 'query', input: { query: 'repo:a b' } }, ORIGIN)).toBe(
			'```sourcegull\nrepo:a b\n```'
		);
	});

	it('serializes file blocks with and without a range', () => {
		const input = { repositoryName: REPO, revision: 'feature', filePath: 'client/web/index.ts', lineRange: null };
		expect(serializeBlock({ type: 'file', input }, ORIGIN)).toBe(
			`${ORIGIN}/${REPO}@feature/-/blob/client/web/index.ts`
		);
		expect(
			serializeBlock({ type: 'file', input: { ...input, revision: '', lineRange: { startLine: 100, endLine: 123 } } }, ORIGIN)
		).toBe(`${ORIGIN}/${REPO}/-/blob/client/web/index.ts?L101-123`);
	});

	it('serializes symbol blocks at their resolved location', () => {
		const block = {
			type: 'symbol' as const,
			input: {
				repositoryName: REPO,
				revision: 'feature',
				filePath: 'client/web/index.ts',
				symbolName: 'func a',
				symbolContainerName: 'class',
				symbolKind: 'FUNCTION',
				lineContext: 3
			}
		};
		const resolved = {
			revision: 'effective-feature',
			filePath: 'client/web/index.ts',
			range: { startLine: 1, startCol: 1, endLine: 1, endCol: 3 }
		};
		expect(serializeBlock(block, ORIGIN, resolved)).toBe(
			`${ORIGIN}/${REPO}@effective-feature/-/blob/client/web/index.ts?L1:1-1:3#symbolName=func+a&symbolContainerName=class&symbolKind=FUNCTION&lineContext=3`
		);
		// Not resolved (yet, or gone): the recorded location, so the block survives export.
		expect(serializeBlock(block, ORIGIN)).toBe(
			`${ORIGIN}/${REPO}@feature/-/blob/client/web/index.ts#symbolName=func+a&symbolContainerName=class&symbolKind=FUNCTION&lineContext=3`
		);
	});

	it('serializes and parses line ranges', () => {
		expect(serializeLineRange({ startLine: 123, endLine: 124 })).toBe('124');
		expect(serializeLineRange({ startLine: 123, endLine: 321 })).toBe('124-321');
		expect(parseBlockLineRange('124')).toEqual({ startLine: 123, endLine: 124 });
		expect(parseBlockLineRange('124-321')).toEqual({ startLine: 123, endLine: 321 });
		expect(parseBlockLineRange('x')).toBeNull();
		expect(parseBlockLineRange('5-2')).toBeNull();
	});

	it('names export files after the title', () => {
		expect(notebookFileName('My notebook: v2!')).toBe('My_notebook_v2_.snb.md');
		expect(titleFromFileName('My_notebook.snb.md')).toBe('My_notebook');
	});
});

describe('.snb.md import', () => {
	it('handles empty and Markdown-only input', () => {
		expect(markdownToBlocks('')).toEqual([]);
		const md = '# Title\n\n## Second title\n\n* L1\n* L2\n\nParagraph';
		expect(markdownToBlocks(md)).toEqual([{ type: 'md', input: { text: md } }]);
	});

	it('handles a single file link', () => {
		expect(markdownToBlocks(`${ORIGIN}/${REPO}@feature/-/blob/client/web/index.ts`)).toEqual([
			{
				type: 'file',
				input: { repositoryName: REPO, revision: 'feature', filePath: 'client/web/index.ts', lineRange: null }
			}
		]);
	});

	it('splits interleaved Markdown, query, file and symbol blocks', () => {
		const url = `https://sourcegull.example.com/gitlab.example.com/acme/app@feature/-/blob/client/web/index.ts`;
		const markdown = `# Title

\`\`\`sourcegull
my query
\`\`\`

## Second title

Paragraph with list:

* 1
* 2
* 3

\`\`\`sourcegull
my second query
\`\`\`

${url}

## Second title v2

Link to a file is inside text ${url}

${url}?L101-123

${url}?L101

\`\`\`go
${url}
\`\`\`

### Third title

https://example.com/a/b

### Symbol block

https://sourcegull.example.com/gitlab.example.com/acme/app@branch/-/blob/client/web/index.ts?L1:1-1:3#symbolName=func+a&symbolContainerName=class&symbolKind=FUNCTION&lineContext=3

${url}#L7-9
`;
		const file = (lineRange: { startLine: number; endLine: number } | null) => ({
			type: 'file',
			input: {
				repositoryName: 'gitlab.example.com/acme/app',
				revision: 'feature',
				filePath: 'client/web/index.ts',
				lineRange
			}
		});
		expect(markdownToBlocks(markdown)).toEqual([
			{ type: 'md', input: { text: '# Title' } },
			{ type: 'query', input: { query: 'my query' } },
			{ type: 'md', input: { text: '## Second title\n\nParagraph with list:\n\n* 1\n* 2\n* 3' } },
			{ type: 'query', input: { query: 'my second query' } },
			file(null),
			{ type: 'md', input: { text: `## Second title v2\n\nLink to a file is inside text ${url}` } },
			file({ startLine: 100, endLine: 123 }),
			file({ startLine: 100, endLine: 101 }),
			{
				type: 'md',
				input: {
					text: `\`\`\`go\n${url}\n\`\`\`\n\n### Third title\n\nhttps://example.com/a/b\n\n### Symbol block`
				}
			},
			{
				type: 'symbol',
				input: {
					repositoryName: 'gitlab.example.com/acme/app',
					revision: 'branch',
					filePath: 'client/web/index.ts',
					symbolName: 'func a',
					symbolContainerName: 'class',
					symbolKind: 'FUNCTION',
					lineContext: 3
				}
			},
			file({ startLine: 6, endLine: 9 })
		]);
	});

	it('ignores links that are not file pages', () => {
		expect(blockFromURL('https://example.com/a/b')).toBeNull();
		expect(blockFromURL(`${ORIGIN}/${REPO}/-/tree/src`)).toBeNull();
		expect(blockFromURL('javascript:alert(1)')).toBeNull();
	});

	it('round-trips a notebook', () => {
		const blocks: Block[] = [
			{ id: '1', type: 'md', input: { text: '# Runbook\n\nSteps:\n\n1. one\n2. two' } },
			{ id: '2', type: 'query', input: { query: 'repo:^gitlab\\.com/acme/web$ lang:go "fmt.Println"' } },
			{ id: '3', type: 'query', input: { query: '' } },
			{
				id: '4',
				type: 'file',
				input: { repositoryName: REPO, revision: '', filePath: 'cmd/my file+x.go', lineRange: { startLine: 9, endLine: 20 } }
			},
			{
				id: '5',
				type: 'file',
				input: { repositoryName: REPO, revision: 'v1.2.0', filePath: 'README.md', lineRange: { startLine: 4, endLine: 5 } }
			},
			{
				id: '6',
				type: 'symbol',
				input: {
					repositoryName: REPO,
					revision: 'main',
					filePath: 'pkg/a.go',
					symbolName: 'Handler',
					symbolContainerName: 'server',
					symbolKind: 'STRUCT',
					lineContext: 5
				}
			},
			{ id: '7', type: 'md', input: { text: 'Done, see ```code``` and\n\n```go\nfunc main() {}\n```' } }
		];
		const text = serializeNotebook(blocks, ORIGIN);
		const back = markdownToBlocks(text);
		expect(back).toEqual(blocks.map(({ id: _id, ...rest }) => rest));
		// Export → import → export is stable.
		expect(serializeNotebook(back.map((b, i) => ({ ...b, id: String(i) })), ORIGIN)).toBe(text);
	});
});

describe('GraphQL block conversion', () => {
	it('converts blocks to inputs and back', () => {
		const blocks: Block[] = [
			{ id: 'a', type: 'md', input: { text: 'x' } },
			{ id: 'b', type: 'query', input: { query: 'y' } },
			{ id: 'c', type: 'file', input: { repositoryName: REPO, revision: '', filePath: 'p', lineRange: null } },
			{
				id: 'd',
				type: 'symbol',
				input: {
					repositoryName: REPO,
					revision: 'r',
					filePath: 'p',
					symbolName: 'S',
					symbolContainerName: '',
					symbolKind: 'CLASS',
					lineContext: 2
				}
			}
		];
		const inputs = blocks.map(blockToInput);
		expect(inputs[0]).toEqual({ id: 'a', type: 'MARKDOWN', markdownInput: 'x' });
		expect(inputs[2]).toEqual({
			id: 'c',
			type: 'FILE',
			fileInput: { repositoryName: REPO, revision: null, filePath: 'p', lineRange: null }
		});
		const nodes = [
			{ __typename: 'MarkdownBlock' as const, id: 'a', markdownInput: 'x' },
			{ __typename: 'QueryBlock' as const, id: 'b', queryInput: 'y' },
			{
				__typename: 'FileBlock' as const,
				id: 'c',
				fileInput: { repositoryName: REPO, revision: null, filePath: 'p', lineRange: null }
			},
			{
				__typename: 'SymbolBlock' as const,
				id: 'd',
				symbolInput: {
					repositoryName: REPO,
					revision: 'r',
					filePath: 'p',
					symbolName: 'S',
					symbolContainerName: '',
					symbolKind: 'CLASS',
					lineContext: 2
				}
			}
		];
		expect(nodes.map(inputToBlock)).toEqual(blocks);
	});
});

describe('symbol resolution', () => {
	const input = {
		repositoryName: 'gitlab.com/acme/web',
		revision: 'main',
		filePath: 'pkg/a.go',
		symbolName: 'Handler',
		symbolContainerName: 'server',
		symbolKind: 'STRUCT',
		lineContext: 3
	};
	const sym = (name: string, kind: string, container: string, line: number) => ({
		url: `/gitlab.com/acme/web@abc/-/blob/x#L${line}:6-${line}:${6 + name.length}`,
		name,
		containerName: container,
		kind,
		line
	});
	const match = (path: string, symbols: ReturnType<typeof sym>[]) => ({
		type: 'symbol' as const,
		path,
		repositoryID: 1,
		repository: 'gitlab.com/acme/web',
		commit: 'abc',
		symbols
	});

	it('builds an exact symbol query', () => {
		expect(symbolSearchQuery(input, true)).toBe(
			'repo:^gitlab\\.com/acme/web$@main file:^pkg/a\\.go$ type:symbol count:100 ^Handler$'
		);
		expect(symbolSearchQuery({ ...input, revision: '' }, false)).toBe(
			'repo:^gitlab\\.com/acme/web$ type:symbol count:100 ^Handler$'
		);
	});

	it('prefers the recorded file, kind and container', () => {
		const found = pickSymbol(input, [
			match('pkg/b.go', [sym('Handler', 'STRUCT', 'server', 3)]),
			match('pkg/a.go', [sym('Handler', 'FUNCTION', '', 9), sym('Handler', 'STRUCT', 'server', 20)])
		]);
		expect(found).toEqual({ commit: 'abc', path: 'pkg/a.go', line: 20, startCol: 6, endCol: 13 });
		// Moved to another file.
		expect(pickSymbol(input, [match('pkg/b.go', [sym('Handler', 'STRUCT', 'server', 3)])])?.path).toBe('pkg/b.go');
		// Gone.
		expect(pickSymbol(input, [match('pkg/a.go', [sym('Other', 'STRUCT', 'server', 3)])])).toBeNull();
		expect(symbolURLRange('/r/-/blob/p#L12:3-12:9')).toEqual({ line: 12, startCol: 3, endCol: 9 });
	});
});
