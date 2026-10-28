import { describe, expect, it } from 'vitest';
import { GraphQLRequestError } from '#lib/graphql.ts';
import {
	SPEC_EXAMPLES,
	batchChangeURLFromRoute,
	batchesUnavailableMessage,
	canRun,
	canWriteBatchChanges,
	editURL,
	executionURL,
	formatIssue,
	initialSpec,
	isEmptySpec,
	isResolving,
	isSpecExecuted,
	isValidBatchChangeName,
	issueRange,
	latestSpec,
	skipReasons,
	specIssues,
	specName,
	specSaveMode,
	withSpecName,
	type EditorSpec
} from './spec.ts';

const stats = { errored: 0, completed: 0, processing: 0, queued: 0, ignored: 0 };

function spec(overrides: Partial<EditorSpec> = {}): EditorSpec {
	return {
		id: 'S1',
		state: 'PENDING',
		source: 'REMOTE',
		originalInput: 'name: x\n',
		createdAt: '2026-10-20T10:00:00Z',
		startedAt: null,
		finishedAt: null,
		applyURL: null,
		viewerCanAdminister: true,
		allowIgnored: false,
		allowUnsupported: false,
		noCache: false,
		failureMessage: null,
		creator: { id: 'U1', username: 'alice' },
		workspaceResolution: {
			state: 'COMPLETED',
			failureMessage: null,
			startedAt: null,
			finishedAt: null,
			workspaces: { totalCount: 2, stats },
			skipped: { totalCount: 0 }
		},
		...overrides
	};
}

describe('access', () => {
	it('needs BATCH_CHANGES#WRITE to write', () => {
		expect(canWriteBatchChanges(null)).toBe(false);
		expect(canWriteBatchChanges({ permissions: { nodes: [{ namespace: 'BATCH_CHANGES', action: 'READ' }] } })).toBe(false);
		expect(canWriteBatchChanges({ permissions: { nodes: [{ namespace: 'BATCH_CHANGES', action: 'WRITE' }] } })).toBe(true);
	});

	it('explains why bulk changes are unavailable', () => {
		const disabled = new GraphQLRequestError('x', 200, [
			{ message: 'batch changes are disabled.', extensions: { code: 'ErrBatchChangesDisabled' } }
		]);
		expect(batchesUnavailableMessage(disabled)).toBe('batch changes are disabled.');
		const forbidden = new GraphQLRequestError('x', 200, [{ message: 'no', extensions: { code: 'FORBIDDEN' } }]);
		expect(batchesUnavailableMessage(forbidden)).toMatch(/permission/);
		expect(batchesUnavailableMessage(new GraphQLRequestError('down', 0))).toBeNull();
		expect(batchesUnavailableMessage(new Error('x'))).toBeNull();
	});
});

describe('URLs and names', () => {
	it('builds the editor and execution URLs', () => {
		const bc = batchChangeURLFromRoute('users', 'alice', 'hello-world');
		expect(bc).toBe('/users/alice/bulk-changes/hello-world');
		expect(editURL(bc)).toBe('/users/alice/bulk-changes/hello-world/edit');
		expect(executionURL(bc, 'QmF0Y2hTcGVj=')).toBe('/users/alice/bulk-changes/hello-world/executions/QmF0Y2hTcGVj%3D');
		expect(batchChangeURLFromRoute('organizations', 'acme', 'a')).toBe('/organizations/acme/bulk-changes/a');
	});

	it('validates bulk change names as specs do', () => {
		expect(isValidBatchChangeName('hello-world_2.0')).toBe(true);
		expect(isValidBatchChangeName('hello world')).toBe(false);
		expect(isValidBatchChangeName('a/b')).toBe(false);
		expect(isValidBatchChangeName('')).toBe(false);
	});
});

describe('save mode', () => {
	const draft = { state: 'DRAFT' as const, currentSpec: { ...spec(), id: 'S0' } };

	it('replaces a server-side spec that has not run', () => {
		expect(specSaveMode(spec(), draft)).toBe('replace');
		// A new draft's empty spec is replaced too.
		expect(specSaveMode(spec({ id: 'S0' }), draft)).toBe('replace');
	});

	it('creates a new spec once the spec ran, for local specs and for applied specs', () => {
		expect(specSaveMode(spec({ startedAt: '2026-10-20T10:01:00Z' }), draft)).toBe('create');
		expect(specSaveMode(spec({ state: 'QUEUED' }), draft)).toBe('create');
		expect(specSaveMode(spec({ source: 'LOCAL' }), draft)).toBe('create');
		expect(specSaveMode(spec({ id: 'S0' }), { ...draft, state: 'OPEN' })).toBe('create');
	});

	it('detects executions from the workspace stats', () => {
		expect(isSpecExecuted(spec())).toBe(false);
		const ran = spec();
		ran.workspaceResolution!.workspaces.stats = { ...stats, completed: 1 };
		expect(isSpecExecuted(ran)).toBe(true);
		expect(isSpecExecuted(spec({ workspaceResolution: null }))).toBe(false);
	});

	it('edits the newest spec', () => {
		const current = spec({ id: 'S0', createdAt: '2026-10-20T09:00:00Z' });
		const newer = spec({ id: 'S1' });
		expect(latestSpec({ currentSpec: current, batchSpecs: { nodes: [newer] } }).id).toBe('S1');
		expect(latestSpec({ currentSpec: current, batchSpecs: { nodes: [] } }).id).toBe('S0');
		const older = spec({ id: 'S2', createdAt: '2026-10-20T08:00:00Z' });
		expect(latestSpec({ currentSpec: current, batchSpecs: { nodes: [older] } }).id).toBe('S0');
	});
});

describe('validation issues', () => {
	it('reads extensions.issues', () => {
		const err = new GraphQLRequestError('x', 200, [
			{
				message: 'line 3, column 5: steps.0: missing property "container"',
				extensions: {
					code: 'VALIDATION',
					issues: [{ message: 'steps.0: missing property "container"', path: '/steps/0', line: 3, column: 5 }]
				}
			}
		]);
		expect(specIssues(err)).toEqual([
			{ message: 'steps.0: missing property "container"', path: '/steps/0', line: 3, column: 5 }
		]);
		expect(formatIssue(specIssues(err)![0]!)).toBe('line 3, column 5: steps.0: missing property "container"');
	});

	it('parses positions from messages without extensions.issues', () => {
		const err = new GraphQLRequestError('x', 200, [
			{
				message: 'The input is invalid:\n- line 1, column 1: name is required\n- something else',
				extensions: { code: 'VALIDATION' }
			}
		]);
		expect(specIssues(err)).toEqual([
			{ message: 'name is required', path: null, line: 1, column: 1 },
			{ message: 'something else', path: null, line: null, column: null }
		]);
		expect(formatIssue({ message: 'm', path: null, line: null, column: null })).toBe('m');
	});

	it('is null for other errors', () => {
		expect(specIssues(new GraphQLRequestError('x', 200, [{ message: 'no', extensions: { code: 'FORBIDDEN' } }]))).toBeNull();
		expect(specIssues(new Error('x'))).toBeNull();
	});

	it('maps positions to document ranges', () => {
		const text = 'name: x\nsteps:\n  - run: echo\n';
		expect(issueRange(text, { line: 3, column: 5 })).toEqual({ from: 19, to: 28 });
		expect(text.slice(19, 28)).toBe('run: echo');
		// Past the end of the line or document: clamped.
		expect(issueRange(text, { line: 1, column: 99 })).toEqual({ from: 7, to: 8 });
		expect(issueRange(text, { line: 99, column: 1 }).from).toBe(text.length);
		expect(issueRange(text, { line: null, column: null })).toEqual({ from: 0, to: 7 });
	});
});

describe('spec text', () => {
	it('reads and sets the name', () => {
		expect(specName('version: 2\nname: hello # comment\n')).toBe('hello');
		expect(specName('name: "quoted"\n')).toBe('quoted');
		expect(specName('steps: []\n')).toBeNull();
		expect(withSpecName('name: a\nsteps: []\n', 'b')).toBe('name: b\nsteps: []\n');
		expect(withSpecName('steps: []\n', 'b')).toBe('name: b\nsteps: []\n');
		// Nested `name:` keys are left alone.
		expect(withSpecName('on:\n  - name: x\n', 'b')).toBe('name: b\non:\n  - name: x\n');
	});

	it('recognises the empty spec of a new bulk change', () => {
		expect(isEmptySpec('name: x\n')).toBe(true);
		expect(isEmptySpec('# comment\nversion: 2\nname: x\n\n')).toBe(true);
		expect(isEmptySpec('name: x\nsteps: []\n')).toBe(false);
	});

	it('starts new bulk changes from the hello world example', () => {
		const code = initialSpec('my-change');
		expect(specName(code)).toBe('my-change');
		expect(code).toContain('changesetTemplate:');
	});

	it('has examples with names and steps', () => {
		expect(SPEC_EXAMPLES.length).toBeGreaterThanOrEqual(5);
		for (const ex of SPEC_EXAMPLES) {
			expect(specName(ex.code), ex.id).not.toBeNull();
			expect(ex.code, ex.id).toContain('steps:');
			expect(specName(withSpecName(ex.code, 'renamed'))).toBe('renamed');
		}
		expect(new Set(SPEC_EXAMPLES.map((e) => e.id)).size).toBe(SPEC_EXAMPLES.length);
	});
});

describe('workspace preview', () => {
	it('gives skip reasons, honouring the flags', () => {
		const ws = { ignored: true, unsupported: true, state: 'SKIPPED' };
		expect(skipReasons(ws)).toEqual(['The repository has a .batchignore file.', "The repository's code host is not supported."]);
		expect(skipReasons(ws, { allowIgnored: true, allowUnsupported: true })).toEqual(['No step runs in this workspace.']);
		expect(skipReasons({ ignored: false, unsupported: false, state: 'PENDING' })).toEqual([]);
	});

	it('runs resolved specs with a workspace that is not skipped', () => {
		expect(canRun(spec())).toBe(true);
		expect(canRun(spec({ viewerCanAdminister: false }))).toBe(false);
		expect(canRun(spec({ workspaceResolution: null }))).toBe(false);
		const skipped = spec();
		skipped.workspaceResolution!.skipped = { totalCount: 2 };
		expect(canRun(skipped)).toBe(false);
		const resolving = spec();
		resolving.workspaceResolution!.state = 'PROCESSING';
		expect(canRun(resolving)).toBe(false);
		expect(isResolving('PROCESSING')).toBe(true);
		expect(isResolving('COMPLETED')).toBe(false);
	});
});
