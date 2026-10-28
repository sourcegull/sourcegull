import { describe, expect, it } from 'vitest';
import {
	formatMs,
	isRunning,
	prettyBody,
	routineHealth,
	statusClass,
	type Routine
} from './logs.ts';

const routine = (over: Partial<Routine>): Routine => ({
	name: 'r',
	type: 'PERIODIC',
	description: '',
	intervalMs: null,
	instances: [],
	recentRuns: [],
	stats: {
		since: null,
		runCount: 0,
		errorCount: 0,
		minDurationMs: 0,
		avgDurationMs: 0,
		maxDurationMs: 0
	},
	queue: null,
	...over
});

describe('admin log helpers', () => {
	it('formats durations', () => {
		expect(formatMs(850)).toBe('850 ms');
		expect(formatMs(2500)).toBe('2.5 s');
		expect(formatMs(45_000)).toBe('45 s');
		expect(formatMs(200_000)).toBe('3 min 20 s');
		expect(formatMs(180_000)).toBe('3 min');
		expect(formatMs(7_500_000)).toBe('2 h 5 min');
	});

	it('tells running instances', () => {
		expect(
			isRunning({
				hostName: 'h',
				lastStartedAt: '2026-01-02T00:00:00Z',
				lastStoppedAt: null
			})
		).toBe(true);
		expect(
			isRunning({
				hostName: 'h',
				lastStartedAt: '2026-01-01T00:00:00Z',
				lastStoppedAt: '2026-01-02T00:00:00Z'
			})
		).toBe(false);
		expect(isRunning({ hostName: 'h', lastStartedAt: null, lastStoppedAt: null })).toBe(false);
	});

	it('summarizes routine health', () => {
		expect(routineHealth(routine({}))).toBe('idle');
		const run = {
			at: '2026-01-01T00:00:00Z',
			hostName: 'h',
			durationMs: 1,
			errorMessage: null
		};
		const live = {
			hostName: 'h',
			lastStartedAt: '2026-01-01T00:00:00Z',
			lastStoppedAt: null
		};
		expect(routineHealth(routine({ instances: [live], recentRuns: [run] }))).toBe('ok');
		expect(routineHealth(routine({ recentRuns: [{ ...run, errorMessage: 'x' }] }))).toBe('failing');
		expect(
			routineHealth(
				routine({
					instances: [{ ...live, lastStoppedAt: '2026-01-02T00:00:00Z' }]
				})
			)
		).toBe('stopped');
		const queue = {
			table: 't',
			queued: 0,
			processing: 0,
			completed: 1,
			errored: 0,
			failed: 1,
			canceled: 0,
			oldestQueuedAt: null,
			recentFailures: [],
			activeJobs: []
		};
		expect(routineHealth(routine({ type: 'DB_BACKED', queue }))).toBe('failing');
	});

	it('classifies statuses and pretty-prints JSON', () => {
		expect(statusClass(200)).toBe('ok');
		expect(statusClass(0)).toBe('bad');
		expect(statusClass(404)).toBe('bad');
		expect(prettyBody('{"a":1}')).toBe('{\n  "a": 1\n}');
		expect(prettyBody('plain')).toBe('plain');
		expect(prettyBody('{broken')).toBe('{broken');
	});
});
