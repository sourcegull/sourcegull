import { describe, expect, it } from 'vitest';
import { parseJsonc, setAt } from '#lib/jsonc-edit.ts';
import {
	MEMBERSHIP_QUERY,
	exclusionsValue,
	groupQuery,
	jsonOnlyKeys,
	lines,
	normalizePath,
	projectEdits,
	readExclusions,
	readProjects,
	tokenPageURL,
	type Edit
} from './gitlab-form.ts';
import { DEFAULT_GITLAB_CONFIG } from './queries.ts';

function apply(text: string, edits: Edit[]): string {
	return edits.reduce((t, [path, value]) => setAt(t, path, value), text);
}

describe('GitLab connection form', () => {
	it('reads the default configuration as membership', () => {
		const doc = parseJsonc(DEFAULT_GITLAB_CONFIG)!;
		expect(readProjects(doc).mode).toBe('membership');
	});

	it('reads groups and projects, and anything else as custom', () => {
		expect(
			readProjects({
				projectQuery: [groupQuery('acme/platform'), groupQuery('tools')],
				projects: [{ name: 'other/api' }, { id: 42 }]
			})
		).toEqual({
			mode: 'selected',
			groups: ['acme/platform', 'tools'],
			projects: ['other/api', '42'],
			queries: [groupQuery('acme/platform'), groupQuery('tools')]
		});
		// No projectQuery means "none": only the explicit projects.
		expect(readProjects({ projects: [{ name: 'a/b' }] }).mode).toBe('selected');
		expect(readProjects({ projectQuery: ['groups/acme/projects'] }).mode).toBe('custom');
		expect(readProjects({ projectQuery: [MEMBERSHIP_QUERY], projects: [{ name: 'a/b' }] }).mode).toBe('custom');
	});

	it('writes a selection and reads it back, keeping comments', () => {
		const text = '{\n  // the instance\n  "url": "https://gitlab.com",\n  "projectQuery": ["none"]\n}\n';
		const sel = { mode: 'selected' as const, groups: ['acme/platform'], projects: ['x/y', '7'], queries: [] };
		const out = apply(text, projectEdits(sel));
		expect(out).toContain('// the instance');
		const doc = parseJsonc(out)!;
		expect(doc.projectQuery).toEqual(['groups/acme%2Fplatform/projects?include_subgroups=true&archived=no']);
		expect(doc.projects).toEqual([{ name: 'x/y' }, { id: 7 }]);
		expect(readProjects(doc)).toMatchObject({ mode: 'selected', groups: ['acme/platform'], projects: ['x/y', '7'] });

		const member = parseJsonc(apply(out, projectEdits({ ...sel, mode: 'membership' })))!;
		expect(member.projectQuery).toEqual([MEMBERSHIP_QUERY]);
		expect(member.projects).toBeUndefined();

		const empty = parseJsonc(apply(out, projectEdits({ mode: 'selected', groups: [], projects: [], queries: [] })))!;
		expect(empty.projectQuery).toBeUndefined();
		expect(empty.projects).toBeUndefined();
	});

	it('normalizes pasted paths and URLs', () => {
		expect(normalizePath(' https://gitlab.example.com/acme/platform/ ')).toBe('acme/platform');
		expect(normalizePath('https://gitlab.com/acme/api/-/tree/main')).toBe('acme/api');
		expect(normalizePath('https://gitlab.com/acme/api.git')).toBe('acme/api');
		expect(normalizePath('/acme/')).toBe('acme');
		expect(lines('a\n\n b \na\n', normalizePath)).toEqual(['a', 'b']);
	});

	it('splits exclusions into the form fields and keeps the rest', () => {
		const doc = {
			exclude: [{ name: 'a/b' }, { pattern: '^x/' }, { emptyRepos: true }, { id: 9 }, { name: 'c/d', emptyRepos: true }]
		};
		const ex = readExclusions(doc);
		expect(ex).toEqual({
			names: ['a/b'],
			patterns: ['^x/'],
			emptyRepos: true,
			other: [{ id: 9 }, { name: 'c/d', emptyRepos: true }]
		});
		expect(exclusionsValue({ ...ex, names: [], patterns: [], emptyRepos: false })).toEqual([
			{ id: 9 },
			{ name: 'c/d', emptyRepos: true }
		]);
		expect(exclusionsValue({ names: [], patterns: [], emptyRepos: false, other: [] })).toBeUndefined();
	});

	it('links to the token page of the instance', () => {
		expect(tokenPageURL('https://gitlab.example.com/', ['read_api'])).toBe(
			'https://gitlab.example.com/-/user_settings/personal_access_tokens?name=Sourcegull&scopes=read_api'
		);
		expect(tokenPageURL('not a url', ['api'])).toContain('https://gitlab.com/-/user_settings/');
	});

	it('lists the settings only the JSON view shows', () => {
		expect(jsonOnlyKeys({ url: 'u', token: 't', webhooks: [], systemHook: true, nameTransformations: [] })).toEqual([
			'systemHook',
			'nameTransformations'
		]);
	});
});
