import { readFileSync } from 'node:fs';
import { buildSchema, parse, validate } from 'graphql';
import { describe, expect, it, vi } from 'vitest';
import {
	BATCH_SIZE,
	codeHostName,
	codeHostURL,
	ExternalURLLoader,
	externalURLsQuery,
	type ExternalLink
} from './codehost.ts';

const gitlab: ExternalLink = { url: 'https://gitlab.example.com/group/project', serviceKind: 'GITLAB' };

describe('codeHostURL', () => {
	it('links the repo to its stored URL', () => {
		expect(codeHostURL(gitlab, { kind: 'repo' })).toBe('https://gitlab.example.com/group/project');
		expect(codeHostURL({ ...gitlab, url: gitlab.url + '/' }, { kind: 'repo' })).toBe(gitlab.url);
	});
	it('deep-links GitLab files, directories and commits', () => {
		expect(codeHostURL(gitlab, { kind: 'blob', rev: 'abc123', path: 'docs/README.md', line: 7 })).toBe(
			'https://gitlab.example.com/group/project/-/blob/abc123/docs/README.md#L7'
		);
		expect(codeHostURL(gitlab, { kind: 'blob', rev: 'feature/x', path: 'a b#.md' })).toBe(
			'https://gitlab.example.com/group/project/-/blob/feature/x/a%20b%23.md'
		);
		expect(codeHostURL(gitlab, { kind: 'tree', rev: 'main', path: '' })).toBe(
			'https://gitlab.example.com/group/project/-/tree/main'
		);
		expect(codeHostURL(gitlab, { kind: 'commit', oid: 'deadbeef' })).toBe(
			'https://gitlab.example.com/group/project/-/commit/deadbeef'
		);
	});
	it('deep-links GitHub files, directories and commits', () => {
		const github: ExternalLink = { url: 'https://github.com/acme/api', serviceKind: 'GITHUB' };
		expect(codeHostURL(github, { kind: 'blob', rev: 'main', path: 'src/lib.rs', line: 3 })).toBe(
			'https://github.com/acme/api/blob/main/src/lib.rs#L3'
		);
		expect(codeHostURL(github, { kind: 'tree', rev: 'v1.0', path: 'docs' })).toBe(
			'https://github.com/acme/api/tree/v1.0/docs'
		);
		expect(codeHostURL(github, { kind: 'commit', oid: 'deadbeef' })).toBe(
			'https://github.com/acme/api/commit/deadbeef'
		);
		expect(codeHostName(github)).toBe('GitHub');
		expect(codeHostName(gitlab)).toBe('GitLab');
	});
	it('deep-links Gitea files, directories and commits', () => {
		const gitea: ExternalLink = { url: 'https://example.com/git/acme/api/', serviceKind: 'GITEA' };
		expect(codeHostURL(gitea, { kind: 'blob', rev: 'v1.0', path: 'src/lib.rs', line: 3 })).toBe(
			'https://example.com/git/acme/api/src/v1.0/src/lib.rs#L3'
		);
		expect(codeHostURL(gitea, { kind: 'tree', rev: '', path: '' })).toBe('https://example.com/git/acme/api/src/HEAD');
		expect(codeHostURL(gitea, { kind: 'commit', oid: 'deadbeef' })).toBe(
			'https://example.com/git/acme/api/commit/deadbeef'
		);
		expect(codeHostName(gitea)).toBe('Gitea');
	});
	it('falls back to the repo page for other hosts', () => {
		const other = { url: 'https://example.com/x', serviceKind: null };
		expect(codeHostURL(other, { kind: 'commit', oid: 'abc' })).toBe('https://example.com/x');
	});
});

describe('externalURLsQuery', () => {
	it('is valid against the schema', () => {
		const schema = buildSchema(readFileSync(new URL('../../../schema.graphql', import.meta.url), 'utf8'));
		expect(validate(schema, parse(externalURLsQuery(3))).map((e) => e.message)).toEqual([]);
	});
});

describe('ExternalURLLoader', () => {
	it('batches names requested together, caches, and maps hidden repos to null', async () => {
		let run: (() => void) | undefined;
		const fetcher = vi.fn(async (_q: string, vars: Record<string, string>) =>
			Object.fromEntries(
				Object.entries(vars).map(([k, name]) => [
					k,
					name === 'hidden' ? null : { externalURLs: [{ url: `https://h/${name}`, serviceKind: 'GITLAB' }] }
				])
			)
		);
		const loader = new ExternalURLLoader(fetcher, (f) => (run = f));
		const names = Array.from({ length: BATCH_SIZE + 1 }, (_, i) => `r${i}`).concat('hidden');
		for (const n of names) expect(loader.get(n)).toBeUndefined();
		loader.get('r0');
		expect(fetcher).not.toHaveBeenCalled();
		run = undefined;
		await loader.flush();
		expect(fetcher).toHaveBeenCalledTimes(2);
		expect(loader.get('r0')).toEqual({ url: 'https://h/r0', serviceKind: 'GITLAB' });
		expect(loader.get('hidden')).toBeNull();
		expect(run).toBeUndefined();
	});
	it('gives no link when the request fails', async () => {
		const loader = new ExternalURLLoader(
			() => Promise.reject(new Error('boom')),
			() => {}
		);
		loader.get('a');
		await loader.flush();
		expect(loader.get('a')).toBeNull();
	});
});
