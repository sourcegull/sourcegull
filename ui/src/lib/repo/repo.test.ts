import { describe, expect, it, vi } from 'vitest';
import { blameStreamURL, relativeDate, SSEParser, streamBlame, type BlameHunk } from './blame.ts';
import { canDelete, canReindex, displayRoot, stateLabel, stepRows } from './codegraph.ts';
import { diffLines, formatBytes } from './diff.ts';
import { decodePacked, kindClass } from './highlight.ts';
import { canWriteMetadata, metadataFilter } from './metadata.ts';
import { emptyRepoBadge, emptyRepoMessage } from './queries.ts';
import {
	ancestorPaths,
	encodePath,
	formatLineRange,
	parseCompareSpec,
	parseLineRange,
	parseRepoRoute,
	repoPageURL,
	repoURL,
	selectLine,
	shortRev,
	withRevision
} from './url.ts';

describe('parseRepoRoute', () => {
	it('parses repository roots with and without a revision', () => {
		expect(parseRepoRoute('/gitlab.com/g/p')).toEqual({
			repo: 'gitlab.com/g/p',
			rev: null,
			kind: 'root',
			path: ''
		});
		expect(parseRepoRoute('/gitlab.com/g/p@v1.0/')).toMatchObject({ rev: 'v1.0', kind: 'root' });
	});

	it('parses page kinds, revisions with slashes and encoded paths', () => {
		expect(parseRepoRoute('/gitlab.com/g/p@feature/x/-/blob/src/a%20b.rs')).toEqual({
			repo: 'gitlab.com/g/p',
			rev: 'feature/x',
			kind: 'blob',
			path: 'src/a b.rs'
		});
		expect(parseRepoRoute('/r/-/tree/')).toMatchObject({ kind: 'tree', path: '' });
		expect(parseRepoRoute('/r/-/commits')).toMatchObject({ kind: 'commits', path: '' });
		expect(parseRepoRoute('/r/-/compare/main...feat')).toMatchObject({
			kind: 'compare',
			path: 'main...feat'
		});
		expect(parseRepoRoute('/r/-/blob/a/-/b')).toMatchObject({ path: 'a/-/b' });
	});

	it('rejects non-repository paths', () => {
		expect(parseRepoRoute('/')).toBeNull();
		expect(parseRepoRoute('/r/-/raw/x')).toBeNull();
		expect(parseRepoRoute('/r/-/unknown')).toBeNull();
		expect(parseRepoRoute('/@main')).toBeNull();
	});
});

describe('URL builders', () => {
	it('round-trips through the parser', () => {
		const url = repoPageURL('gitlab.com/g/p', 'feature/x', 'blob', 'dir/a #1?.rs');
		expect(url).toBe('/gitlab.com/g/p@feature/x/-/blob/dir/a%20%231%3F.rs');
		expect(parseRepoRoute(url)).toEqual({
			repo: 'gitlab.com/g/p',
			rev: 'feature/x',
			kind: 'blob',
			path: 'dir/a #1?.rs'
		});
		expect(repoURL('r', null)).toBe('/r');
		expect(repoPageURL('r', null, 'root')).toBe('/r');
		expect(repoPageURL('r', 'main', 'commits')).toBe('/r@main/-/commits');
		expect(encodePath('a@b+c:d')).toBe('a@b+c:d');
	});

	it('switches revisions keeping the page where it makes sense', () => {
		const blob = parseRepoRoute('/r@main/-/blob/a.rs')!;
		expect(withRevision(blob, 'v1')).toBe('/r@v1/-/blob/a.rs');
		const commit = parseRepoRoute('/r/-/commit/abc')!;
		expect(withRevision(commit, 'v1')).toBe('/r@v1');
	});

	it('lists ancestors and shortens oids', () => {
		expect(ancestorPaths('a/b/c.rs')).toEqual(['a', 'a/b']);
		expect(ancestorPaths('c.rs')).toEqual([]);
		expect(shortRev('0123456789abcdef0123456789abcdef01234567')).toBe('0123456');
		expect(shortRev('main')).toBe('main');
	});
});

describe('line ranges', () => {
	it('parses and formats', () => {
		expect(parseLineRange('#L10')).toEqual({ start: 10, end: 10 });
		expect(parseLineRange('#L20-10')).toEqual({ start: 10, end: 20 });
		expect(parseLineRange('L3:4-L5:6')).toEqual({ start: 3, end: 5 });
		expect(parseLineRange('#L0')).toBeNull();
		expect(parseLineRange('#foo')).toBeNull();
		expect(parseLineRange('')).toBeNull();
		expect(formatLineRange({ start: 4, end: 4 })).toBe('L4');
		expect(formatLineRange({ start: 4, end: 9 })).toBe('L4-9');
	});

	it('extends selections with shift', () => {
		expect(selectLine(null, 5, true)).toEqual({ start: 5, end: 5 });
		expect(selectLine({ start: 5, end: 5 }, 9, true)).toEqual({ start: 5, end: 9 });
		expect(selectLine({ start: 5, end: 9 }, 2, true)).toEqual({ start: 2, end: 9 });
		expect(selectLine({ start: 5, end: 9 }, 7, false)).toEqual({ start: 7, end: 7 });
	});

	it('splits compare specs', () => {
		expect(parseCompareSpec('main...feat/x')).toEqual({ base: 'main', head: 'feat/x' });
		expect(parseCompareSpec('a..b')).toEqual({ base: 'a', head: 'b' });
		expect(parseCompareSpec('b')).toEqual({ base: '', head: 'b' });
	});
});

describe('decodePacked', () => {
	it('decodes relative lines and columns', () => {
		// line 0: [0,2) kw, [3,7) fn; line 2: [4,9) string
		const packed = [0, 0, 2, 2, 0, 3, 4, 9, 2, 4, 5, 3];
		expect(decodePacked(packed)).toEqual([
			{ line: 0, startCol: 0, endCol: 2, kind: 2 },
			{ line: 0, startCol: 3, endCol: 7, kind: 9 },
			{ line: 2, startCol: 4, endCol: 9, kind: 3 }
		]);
		expect(decodePacked([])).toEqual([]);
		expect(() => decodePacked([1, 2, 3])).toThrow();
	});

	it('maps kinds to classes', () => {
		expect(kindClass(1)).toBe('hl-comment');
		expect(kindClass(2)).toBe('hl-keyword');
		expect(kindClass(28)).toBe('hl-code');
		expect(kindClass(0)).toBeNull();
		expect(kindClass(99)).toBeNull();
	});
});

describe('SSE and blame', () => {
	it('parses events split across chunks', () => {
		const p = new SSEParser();
		expect(p.push('event: hunk\ndata: [1')).toEqual([]);
		expect(p.push(']\n\n: keep-alive\n\nevent: done\r\ndata: {}\r\n\r\n')).toEqual([
			{ event: 'hunk', data: '[1]' },
			{ event: 'done', data: '{}' }
		]);
		expect(p.push('data: a\ndata: b\n\n')).toEqual([{ event: 'message', data: 'a\nb' }]);
	});

	it('streams hunks until done', async () => {
		const hunk: BlameHunk = {
			commitID: 'abc',
			startLine: 1,
			endLine: 3,
			author: { Name: 'Ann', Email: 'a@x', Date: '2024-01-01T00:00:00Z' },
			message: 'm',
			filename: 'f',
			commit: { parents: [], url: '/r/-/commit/abc' }
		};
		const body = `event: hunk\ndata: ${JSON.stringify([hunk])}\n\nevent: done\ndata: {}\n\n`;
		const fetchMock = vi.fn<typeof fetch>(
			async () => new Response(body, { headers: { 'Content-Type': 'text/event-stream' } })
		);
		const got: BlameHunk[] = [];
		await streamBlame('/x', (h) => got.push(...h), undefined, fetchMock);
		expect(got).toEqual([hunk]);

		const failing = vi.fn<typeof fetch>(
			async () => new Response(JSON.stringify({ error: 'nope' }), { status: 404 })
		);
		await expect(streamBlame('/x', () => {}, undefined, failing)).rejects.toThrow('nope');
	});

	it('builds the stream URL and relative dates', () => {
		expect(blameStreamURL('g.com/a/b', 'main', 'src/x y.rs')).toBe(
			'/.api/blame/g.com/a/b@main/-/stream/src/x%20y.rs'
		);
		const now = new Date('2024-01-10T00:00:00Z');
		expect(relativeDate('2024-01-09T00:00:00Z', now)).toBe('1 day ago');
		expect(relativeDate('2024-01-03T00:00:00Z', now)).toBe('1 week ago');
		expect(relativeDate('2024-01-10T00:00:00Z', now)).toBe('just now');
	});
});

describe('diffLines', () => {
	it('numbers context, added and deleted lines', () => {
		const body = ' a\n-b\n+c\n+d\n e\n\\ No newline at end of file\n';
		expect(diffLines(body, 10, 20)).toEqual([
			{ kind: 'context', oldLine: 10, newLine: 20, text: 'a' },
			{ kind: 'deleted', oldLine: 11, newLine: null, text: 'b' },
			{ kind: 'added', oldLine: null, newLine: 21, text: 'c' },
			{ kind: 'added', oldLine: null, newLine: 22, text: 'd' },
			{ kind: 'context', oldLine: 12, newLine: 23, text: 'e' },
			{ kind: 'note', oldLine: null, newLine: null, text: '\\ No newline at end of file' }
		]);
		expect(diffLines('', 1, 1)).toEqual([]);
	});

	it('formats sizes', () => {
		expect(formatBytes(12)).toBe('12 B');
		expect(formatBytes(2048)).toBe('2.0 KB');
		expect(formatBytes(50 * 1024 * 1024)).toBe('50 MB');
	});
});

describe('code graph data', () => {
	it('routes', () => {
		expect(parseRepoRoute('/r/-/code-graph')).toMatchObject({ repo: 'r', kind: 'code-graph' });
		expect(repoPageURL('r', null, 'code-graph')).toBe('/r/-/code-graph');
		expect(withRevision(parseRepoRoute('/r/-/code-graph')!, 'v1')).toBe('/r@v1');
		expect(parseRepoRoute('/r/-/settings')).toMatchObject({ repo: 'r', kind: 'settings' });
		expect(repoPageURL('r', null, 'settings')).toBe('/r/-/settings');
	});
	it('labels states and actions', () => {
		expect(stateLabel('PROCESSING_ERRORED')).toEqual({ text: 'Errored', tone: 'danger' });
		expect(canReindex('COMPLETED')).toBe(true);
		expect(canReindex('QUEUED_FOR_PROCESSING')).toBe(false);
		expect(canDelete('DELETING')).toBe(false);
		expect(canDelete('UPLOADING_INDEX')).toBe(true);
		expect(displayRoot('')).toBe('/');
		expect(displayRoot('web/')).toBe('web/');
		expect(canReindex('INDEXING_ERRORED')).toBe(true);
		expect(canReindex('QUEUED_FOR_INDEXING')).toBe(false);
	});
	it('lists an index job\'s steps with their logs', () => {
		const entry = (key: string, exitCode: number) => ({
			key,
			command: ['git', 'fetch'],
			startTime: null,
			exitCode,
			out: 'ok',
			durationMilliseconds: 3
		});
		const rows = stepRows({
			setup: [entry('setup.git.fetch', 0)],
			preIndex: [{ root: '', image: 'node', commands: ['npm install'], logEntry: entry('step.docker.pre-index.0', 0) }],
			index: {
				commands: ['echo hi'],
				indexerArgs: ['scip-typescript', 'index'],
				outfile: 'index.scip',
				requestedEnvVars: null,
				logEntry: null
			},
			upload: null,
			teardown: []
		});
		expect(rows.map((r) => r.label)).toEqual(['Setup', 'Pre-index step 1', 'Indexer']);
		expect(rows[1]?.detail).toBe('node in /: npm install');
		expect(rows[2]?.detail).toBe('echo hi; scip-typescript index');
		expect(rows[2]?.entry).toBeNull();
	});
});

describe('emptyRepoMessage', () => {
	it('explains empty repositories without calling them errors', () => {
		expect(emptyRepoMessage({ isEmpty: false, remoteUnavailable: null })).toBeNull();
		expect(emptyRepoMessage({ isEmpty: true, remoteUnavailable: 'MISSING' })).toBe(
			'This repository is empty (no repository on GitLab yet).'
		);
		expect(emptyRepoMessage({ isEmpty: true, remoteUnavailable: 'DISABLED' })).toBe(
			'Repository is disabled for this project on GitLab.'
		);
		expect(emptyRepoMessage({ isEmpty: true, remoteUnavailable: 'NO_CODE_ACCESS' })).toBe(
			"Sourcegull may not download this repository's code. The code host token needs at least Reporter access."
		);
		expect(emptyRepoMessage({ isEmpty: true, remoteUnavailable: null })).toBe(
			'This repository is empty (no commits yet).'
		);
	});
});

describe('emptyRepoBadge', () => {
	it('labels empty repositories, with a warning only for missing code access', () => {
		expect(emptyRepoBadge({ isEmpty: false, remoteUnavailable: null })).toBeNull();
		expect(emptyRepoBadge({ isEmpty: true, remoteUnavailable: null })).toEqual({
			label: 'empty',
			title: 'No commits yet',
			warning: false
		});
		expect(emptyRepoBadge({ isEmpty: true, remoteUnavailable: 'MISSING' })?.title).toBe(
			'No repository on GitLab yet'
		);
		expect(emptyRepoBadge({ isEmpty: true, remoteUnavailable: 'DISABLED' })).toEqual({
			label: 'repository disabled',
			title: 'Repository is disabled for this project on GitLab',
			warning: false
		});
		expect(emptyRepoBadge({ isEmpty: true, remoteUnavailable: 'NO_CODE_ACCESS' })).toEqual({
			label: 'no code access',
			title: 'The code host token needs at least Reporter access.',
			warning: true
		});
	});
});

describe('repository metadata', () => {
	it('needs REPO_METADATA#WRITE', () => {
		const viewer = (namespace: string, action: string) => ({ id: 'u', permissions: { nodes: [{ namespace, action }] } });
		expect(canWriteMetadata(viewer('REPO_METADATA', 'WRITE'))).toBe(true);
		expect(canWriteMetadata(viewer('BATCH_CHANGES', 'WRITE'))).toBe(false);
		expect(canWriteMetadata(null)).toBe(false);
	});

	it('builds the search filter for a pair', () => {
		expect(metadataFilter('team', 'search')).toBe('repo:has.meta(team:search)');
		expect(metadataFilter('go', null)).toBe('repo:has.tag(go)');
		expect(metadataFilter('a:b', null)).toBeNull();
		expect(metadataFilter('team', 'two words')).toBeNull();
	});
});
