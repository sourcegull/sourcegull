import { describe, expect, it } from 'vitest';
import {
	batchChangeURL,
	burndownFrom,
	bulkOperationSummary,
	compactNumber,
	detailTab,
	isNamespaceKind,
	namespaceName,
	niceTicks,
	statsEntries,
	stateFilterStates,
	type ChangesetsStats
} from './batch-change.ts';
import {
	CHANGESETS,
	bulkMutation,
	changesetBranch,
	csvFileName,
	enumLabel,
	filterVariables,
	stateTone,
	toFileDiffConnection,
	type ExternalChangesetNode
} from './changesets.ts';
import { codeHostName, credentialStatus, type BatchChangesCodeHost } from './credentials.ts';
import { parseInline, parseMarkdown, safeHref } from './markdown.ts';
import {
	APPLY_BATCH_CHANGE,
	CREATE_BATCH_CHANGE,
	applyMutation,
	canSetPublicationState,
	deltaSummary,
	previewTitle,
	publicationStatesInput,
	statsOperations,
	type PreviewSpec,
	type PreviewStats,
	type PreviewTargets
} from './preview.ts';

const stats: ChangesetsStats = {
	unpublished: 2,
	draft: 0,
	open: 1,
	merged: 3,
	closed: 0,
	deleted: 0,
	retrying: 0,
	failed: 1,
	scheduled: 0,
	processing: 0,
	archived: 1,
	total: 8,
	isCompleted: false,
	percentComplete: 60
};

describe('bulk changes', () => {
	it('builds URLs like BatchChange.url', () => {
		expect(batchChangeURL('users', 'alice', 'hello world')).toBe('/users/alice/bulk-changes/hello%20world');
		expect(isNamespaceKind('organizations')).toBe(true);
		expect(isNamespaceKind('repos')).toBe(false);
	});

	it('names namespaces', () => {
		expect(namespaceName({ __typename: 'User', id: 'u', username: 'alice', url: '/users/alice' })).toBe('alice');
		expect(namespaceName({ __typename: 'Org', id: 'o', name: 'acme', url: '/organizations/acme' })).toBe('acme');
	});

	it('picks the tab and the state filter', () => {
		expect(detailTab(null)).toBe('changesets');
		expect(detailTab('chart')).toBe('chart');
		expect(detailTab('nope')).toBe('changesets');
		expect(stateFilterStates('all')).toBeNull();
		expect(stateFilterStates('open')).toEqual(['OPEN']);
	});

	it('lists stats with open, merged and closed always', () => {
		expect(statsEntries(stats).map((e) => `${e.label} ${e.count}`)).toEqual([
			'Failed 1',
			'Unpublished 2',
			'Open 1',
			'Merged 3',
			'Closed 0',
			'Archived 1'
		]);
	});

	it('computes clean ticks and compact labels', () => {
		expect(niceTicks(0)).toEqual([0, 1]);
		expect(niceTicks(3)).toEqual([0, 1, 2, 3]);
		expect(niceTicks(7)).toEqual([0, 2, 4, 6, 8]);
		expect(niceTicks(130)).toEqual([0, 50, 100, 150]);
		expect(compactNumber(950)).toBe('950');
		expect(compactNumber(1500)).toBe('1.5K');
		expect(compactNumber(12000)).toBe('12K');
	});

	it('computes burndown ranges', () => {
		const now = new Date('2026-10-20T00:00:00Z');
		expect(burndownFrom('all', now)).toBeNull();
		expect(burndownFrom('7d', now)).toBe('2026-10-13T00:00:00.000Z');
	});

	it('summarises bulk operations', () => {
		expect(bulkOperationSummary('COMMENT', 1)).toBe('Commented on 1 changeset');
		expect(bulkOperationSummary('REENQUEUE', 3)).toBe('Retried 3 changesets');
	});
});

describe('changesets', () => {
	it('turns filters into variables', () => {
		expect(
			filterVariables({ state: 'OPEN', reviewState: '', checkState: 'FAILED', search: '  fix ', onlyArchived: false })
		).toEqual({ state: 'OPEN', reviewState: null, checkState: 'FAILED', search: 'fix', onlyArchived: false });
		expect(CHANGESETS).toContain('onlyArchived: $onlyArchived');
	});

	it('labels states', () => {
		expect(enumLabel('CHANGES_REQUESTED')).toBe('Changes requested');
		expect(stateTone('MERGED')).toBe('merged');
		expect(stateTone('FAILED')).toBe('danger');
		expect(stateTone('UNPUBLISHED')).toBe('muted');
	});

	it('reads the branch from the spec', () => {
		const c = {
			currentSpec: { id: 's', description: { __typename: 'GitBranchChangesetDescription', baseRef: 'main', headRef: 'fix', published: null } }
		} as ExternalChangesetNode;
		expect(changesetBranch(c)).toBe('fix');
		expect(changesetBranch({ currentSpec: null } as ExternalChangesetNode)).toBeNull();
	});

	it('builds bulk mutations with their options', () => {
		const m = bulkMutation('MERGE', 'bc', ['a', 'b'], { squash: true });
		expect(m.query).toContain('mergeChangesets');
		expect(m.variables).toEqual({ batchChange: 'bc', changesets: ['a', 'b'], squash: true });
		expect(bulkMutation('COMMENT', 'bc', ['a'], { body: 'hi' }).variables['body']).toBe('hi');
		expect(bulkMutation('PUBLISH', 'bc', ['a'], { draft: true }).variables['draft']).toBe(true);
		expect(bulkMutation('DETACH', 'bc', ['a']).variables).toEqual({ batchChange: 'bc', changesets: ['a'] });
	});

	it('maps spec diffs to DiffView file statuses', () => {
		const stat = { added: 1, deleted: 0 };
		const d = toFileDiffConnection({
			totalCount: 4,
			diffStat: stat,
			nodes: [
				{ oldPath: null, newPath: 'a', binary: false, stat, hunks: [] },
				{
					oldPath: 'b',
					newPath: null,
					binary: false,
					stat,
					hunks: [],
					mostRelevantFile: { __typename: 'GitBlob', url: '/r/-/blob/b' }
				},
				{ oldPath: 'c', newPath: 'd', binary: false, stat, hunks: [] },
				{ oldPath: 'e', newPath: 'e', binary: false, stat, hunks: [] }
			]
		});
		expect(d.nodes.map((n) => n.status)).toEqual(['ADDED', 'DELETED', 'RENAMED', 'MODIFIED']);
		// Only the deleted file has a page (at the base revision).
		expect(d.nodes.map((n) => n.url)).toEqual([null, '/r/-/blob/b', null, null]);
	});

	it('names CSV files safely', () => {
		expect(csvFileName('update deps/2026')).toBe('update-deps-2026-changesets.csv');
	});
});

const branchSpec = (published: unknown): PreviewSpec => ({
	id: 'spec1',
	type: 'BRANCH',
	description: {
		__typename: 'GitBranchChangesetDescription',
		baseRepository: { name: 'gitlab.example.com/acme/app', url: '/gitlab.example.com/acme/app' },
		baseRef: 'main',
		headRef: 'hello',
		title: 'Hello',
		body: '',
		published: published as boolean | 'draft' | null,
		diffStat: { added: 1, deleted: 0 }
	}
});

describe('preview', () => {
	it('allows publication states only where the spec leaves published unset', () => {
		const attach = (p: unknown): PreviewTargets => ({
			__typename: 'VisibleApplyPreviewTargetsAttach',
			changesetSpec: branchSpec(p)
		});
		expect(canSetPublicationState(attach(null))).toBe(true);
		expect(canSetPublicationState(attach(false))).toBe(false);
		expect(canSetPublicationState(attach('draft'))).toBe(false);
		const detach: PreviewTargets = {
			__typename: 'VisibleApplyPreviewTargetsDetach',
			changeset: {
				id: 'c',
				state: 'OPEN',
				title: 'Old',
				externalID: '7',
				externalURL: null,
				repository: { name: 'r', url: '/r' }
			}
		};
		expect(canSetPublicationState(detach)).toBe(false);
		expect(previewTitle(detach)).toEqual({ title: 'Old', repo: 'r', branch: null });
		expect(previewTitle(attach(null))).toEqual({ title: 'Hello', repo: 'gitlab.example.com/acme/app', branch: 'hello' });
	});

	it('builds publicationStates', () => {
		const m = new Map<string, boolean | 'draft'>([
			['a', true],
			['b', 'draft']
		]);
		expect(publicationStatesInput(m)).toEqual([
			{ changesetSpec: 'a', publicationState: true },
			{ changesetSpec: 'b', publicationState: 'draft' }
		]);
	});

	it('applies with ensureBatchChange or creates', () => {
		const update = applyMutation(
			{ id: 'spec', appliesToBatchChange: { id: 'bc', name: 'n', url: '/u', state: 'OPEN' } },
			[{ changesetSpec: 'a', publicationState: true }]
		);
		expect(update.query).toBe(APPLY_BATCH_CHANGE);
		expect(update.field).toBe('applyBatchChange');
		expect(update.variables['ensureBatchChange']).toBe('bc');
		const create = applyMutation({ id: 'spec', appliesToBatchChange: null }, []);
		expect(create.query).toBe(CREATE_BATCH_CHANGE);
		expect(create.variables['publicationStates']).toBeNull();
	});

	it('summarises deltas and stats', () => {
		expect(
			deltaSummary({
				titleChanged: true,
				bodyChanged: false,
				undraft: false,
				baseRefChanged: false,
				diffChanged: true,
				commitMessageChanged: false,
				authorNameChanged: false,
				authorEmailChanged: true
			})
		).toEqual(['title', 'diff', 'commit author']);
		const s = { publish: 2, archive: 1 } as Partial<PreviewStats>;
		const full = Object.fromEntries(
			['push', 'update', 'undraft', 'publish', 'publishDraft', 'sync', 'import', 'close', 'reopen', 'sleep', 'detach', 'archive', 'reattach', 'added', 'modified', 'removed'].map(
				(k) => [k, (s as Record<string, number>)[k] ?? 0]
			)
		) as unknown as PreviewStats;
		expect(statsOperations(full)).toEqual([
			{ op: 'PUBLISH', count: 2 },
			{ op: 'ARCHIVE', count: 1 }
		]);
	});
});

describe('credentials', () => {
	const host = (credential: BatchChangesCodeHost['credential']): BatchChangesCodeHost => ({
		externalServiceKind: 'GITLAB',
		externalServiceURL: 'https://gitlab.example.com/',
		requiresSSH: false,
		requiresUsername: false,
		hasWebhooks: false,
		credential
	});

	it('tells own, site and missing credentials apart', () => {
		const site = { id: 'c', isSiteCredential: true, createdAt: '', sshPublicKey: null };
		expect(credentialStatus(host(null), 'user')).toBe('none');
		expect(credentialStatus(host(site), 'user')).toBe('site');
		expect(credentialStatus(host(site), 'site')).toBe('own');
		expect(credentialStatus(host({ ...site, isSiteCredential: false }), 'user')).toBe('own');
		expect(codeHostName('https://gitlab.example.com/')).toBe('gitlab.example.com');
	});
});

describe('markdown', () => {
	it('parses blocks', () => {
		const blocks = parseMarkdown('# Title\n\nSome **bold** text.\n\n- one\n- two\n\n```\ncode\n```\n\n> quote');
		expect(blocks.map((b) => b.kind)).toEqual(['heading', 'para', 'list', 'code', 'quote']);
		const list = blocks[2];
		expect(list?.kind === 'list' && list.items.length).toBe(2);
	});

	it('parses inline markup and drops unsafe links', () => {
		expect(parseInline('a `b` *c* [d](https://x.y)')).toEqual([
			{ kind: 'text', text: 'a ' },
			{ kind: 'code', text: 'b' },
			{ kind: 'text', text: ' ' },
			{ kind: 'em', children: [{ kind: 'text', text: 'c' }] },
			{ kind: 'text', text: ' ' },
			{ kind: 'link', href: 'https://x.y', children: [{ kind: 'text', text: 'd' }] }
		]);
		expect(parseInline('[x](javascript:alert(1))')).toEqual([{ kind: 'text', text: 'x' }, { kind: 'text', text: ')' }]);
		expect(safeHref('javascript:alert(1)')).toBeNull();
		expect(safeHref('/users/alice')).toBe('/users/alice');
		expect(parseInline('see https://example.com.')[1]).toEqual({
			kind: 'link',
			href: 'https://example.com',
			children: [{ kind: 'text', text: 'https://example.com' }]
		});
	});
});
