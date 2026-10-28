import { StringStream } from '@codemirror/language';
import { describe, expect, it } from 'vitest';
import { yamlStreamParser } from '#lib/editor/yaml.ts';
import {
	canCancel,
	canModifyExecution,
	canRetryWorkspace,
	elapsed,
	executionProgress,
	formatDuration,
	formatOutputValue,
	isExecutionActive,
	parseOutputLine,
	queuePosition,
	stateLabel,
	stateTone,
	stepStatus,
	workspaceTitle
} from './execution.ts';

describe('execution', () => {
	const alice = { id: 'U1', siteAdmin: false };
	const admin = { id: 'U9', siteAdmin: true };
	const bob = { id: 'U2', siteAdmin: false };

	it('lets the creator and site admins cancel and retry', () => {
		const spec = { creator: { id: 'U1', username: 'alice' }, state: 'PROCESSING' as const };
		expect(canModifyExecution(spec, alice)).toBe(true);
		expect(canModifyExecution(spec, admin)).toBe(true);
		expect(canModifyExecution(spec, bob)).toBe(false);
		expect(canModifyExecution(spec, null)).toBe(false);
		expect(canModifyExecution({ creator: null }, alice)).toBe(false);
		expect(canCancel(spec, alice)).toBe(true);
		expect(canCancel({ ...spec, state: 'COMPLETED' }, alice)).toBe(false);
		expect(canCancel(spec, bob)).toBe(false);
	});

	it('retries finished visible workspaces', () => {
		expect(canRetryWorkspace({ __typename: 'VisibleBatchSpecWorkspace', state: 'FAILED' })).toBe(true);
		expect(canRetryWorkspace({ __typename: 'VisibleBatchSpecWorkspace', state: 'COMPLETED' })).toBe(true);
		expect(canRetryWorkspace({ __typename: 'VisibleBatchSpecWorkspace', state: 'QUEUED' })).toBe(false);
		expect(canRetryWorkspace({ __typename: 'HiddenBatchSpecWorkspace', state: 'FAILED' })).toBe(false);
	});

	it('computes progress over the workspaces that run', () => {
		const stats = { completed: 2, errored: 1, processing: 1, queued: 1 };
		expect(executionProgress(stats, 7, 2)).toEqual({ done: 3, total: 5, ratio: 0.6 });
		expect(executionProgress({ completed: 0, errored: 0, processing: 0, queued: 0 }, 2, 2)).toEqual({
			done: 0,
			total: 0,
			ratio: 1
		});
	});

	it('knows active states and labels them', () => {
		expect(isExecutionActive('QUEUED')).toBe(true);
		expect(isExecutionActive('CANCELING')).toBe(true);
		expect(isExecutionActive('FAILED')).toBe(false);
		expect(stateLabel('PROCESSING')).toBe('Processing');
		expect(stateTone('COMPLETED')).toBe('success');
		expect(stateTone('FAILED')).toBe('danger');
		expect(stateTone('QUEUED')).toBe('info');
		expect(stateTone('CANCELED')).toBe('warning');
		expect(stateTone('SKIPPED')).toBe('muted');
	});

	it('formats durations', () => {
		expect(formatDuration(850)).toBe('850 ms');
		expect(formatDuration(12_340)).toBe('12.3 s');
		expect(formatDuration(245_000)).toBe('4m 05s');
		expect(formatDuration(7_380_000)).toBe('2h 03m');
		expect(elapsed(null, null)).toBeNull();
		expect(elapsed('2026-10-20T10:00:00Z', '2026-10-20T10:00:05Z')).toBe(5000);
		expect(elapsed('2026-10-20T10:00:00Z', null, Date.parse('2026-10-20T10:01:00Z'))).toBe(60_000);
	});

	it('splits output lines by stream', () => {
		expect(parseOutputLine('stdout: hello')).toEqual({ stream: 'stdout', text: 'hello' });
		expect(parseOutputLine('stderr: oops: bad')).toEqual({ stream: 'stderr', text: 'oops: bad' });
		expect(parseOutputLine('stdout: ')).toEqual({ stream: 'stdout', text: '' });
		expect(parseOutputLine('plain')).toEqual({ stream: null, text: 'plain' });
	});

	it('describes steps', () => {
		const base = { skipped: false, cachedResultFound: false, exitCode: null, startedAt: null, finishedAt: null };
		expect(stepStatus({ ...base, skipped: true })).toEqual({ label: 'skipped', tone: 'muted' });
		expect(stepStatus({ ...base, cachedResultFound: true })).toEqual({ label: 'cached', tone: 'success' });
		expect(stepStatus({ ...base, exitCode: 0 })).toEqual({ label: 'exit 0', tone: 'success' });
		expect(stepStatus({ ...base, exitCode: 2 })).toEqual({ label: 'exit 2', tone: 'danger' });
		expect(stepStatus({ ...base, startedAt: '2026-10-20T10:00:00Z' })).toEqual({ label: 'running', tone: 'info' });
		expect(stepStatus(base)).toEqual({ label: 'not run', tone: 'muted' });
		expect(formatOutputValue('text')).toBe('text');
		expect(formatOutputValue({ a: 1 })).toBe('{\n  "a": 1\n}');
		expect(formatOutputValue(null)).toBe('null');
	});

	it('describes workspaces', () => {
		expect(queuePosition({ state: 'QUEUED', placeInQueue: 2, placeInGlobalQueue: 5 })).toBe('#2 in your queue, #5 overall');
		expect(queuePosition({ state: 'PROCESSING', placeInQueue: 1, placeInGlobalQueue: 1 })).toBeNull();
		expect(
			workspaceTitle({ repository: { name: 'g/a', url: '/g/a' }, branch: { displayName: 'main' }, path: 'web' })
		).toBe('g/a@main · web');
		expect(workspaceTitle({ repository: { name: 'g/a', url: '/g/a' }, branch: { displayName: 'main' }, path: '' })).toBe(
			'g/a@main'
		);
		expect(workspaceTitle({})).toBe('Hidden repository');
	});
});

/** Tokens of a YAML document as `[text, token]` pairs (whitespace dropped). */
function tokens(doc: string): [string, string | null][] {
	const state = yamlStreamParser.startState!(2);
	const out: [string, string | null][] = [];
	for (const line of doc.split('\n')) {
		const stream = new StringStream(line, 2, 2);
		if (line === '') yamlStreamParser.blankLine?.(state, 2);
		while (!stream.eol()) {
			const style = yamlStreamParser.token(stream, state);
			const text = stream.current();
			if (text.trim() !== '') out.push([text, style]);
			stream.start = stream.pos;
		}
	}
	return out;
}

describe('YAML highlighting', () => {
	it('tokenizes keys, scalars, comments and templates', () => {
		expect(tokens('name: hello # the name\non:\n  - repository: gitlab.example.com/a/b')).toEqual([
			['name', 'propertyName'],
			[':', 'punctuation'],
			['hello', 'string'],
			['# the name', 'comment'],
			['on', 'propertyName'],
			[':', 'punctuation'],
			['-', 'punctuation'],
			['repository', 'propertyName'],
			[':', 'punctuation'],
			['gitlab.example.com/a/b', 'string']
		]);
		expect(tokens('branch: x-${{ outputs.name }}\nn: 12\nb: true')).toEqual([
			['branch', 'propertyName'],
			[':', 'punctuation'],
			['x-', 'string'],
			['${{ outputs.name }}', 'meta'],
			['n', 'propertyName'],
			[':', 'punctuation'],
			['12', 'number'],
			['b', 'propertyName'],
			[':', 'punctuation'],
			['true', 'bool']
		]);
	});

	it('treats block scalars as strings until the indentation drops', () => {
		const t = tokens('steps:\n  - run: |\n      echo a: b\n\n      # not a comment\n    container: alpine');
		expect(t).toContainEqual(['|', 'operator']);
		expect(t).toContainEqual(['      echo a: b', 'string']);
		expect(t).toContainEqual(['      # not a comment', 'string']);
		expect(t).toContainEqual(['container', 'propertyName']);
	});
});
