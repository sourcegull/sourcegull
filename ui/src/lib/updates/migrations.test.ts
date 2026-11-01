import { describe, expect, it } from 'vitest';
import { migrationState, percent, runningMigrations } from './migrations.ts';

describe('out-of-band migration helpers', () => {
	it('rounds progress down', () => {
		expect(percent(0)).toBe(0);
		expect(percent(0.999)).toBe(99);
		expect(percent(1)).toBe(100);
		expect(percent(1.5)).toBe(100);
		expect(percent(-1)).toBe(0);
	});

	it('names the state', () => {
		expect(migrationState({ progress: 0.4, applyReverse: false })).toBe('Running');
		expect(migrationState({ progress: 1, applyReverse: false })).toBe('Complete');
		expect(migrationState({ progress: 0.4, applyReverse: true })).toBe('Reverting');
		expect(migrationState({ progress: 0, applyReverse: true })).toBe('Reverted');
	});

	it('keeps the running migrations', () => {
		const all = [
			{ id: 'a', progress: 0.4, applyReverse: false },
			{ id: 'b', progress: 1, applyReverse: false },
			{ id: 'c', progress: 0.4, applyReverse: true },
			{ id: 'd', progress: 0, applyReverse: true }
		];
		expect(runningMigrations(all).map((m) => m.id)).toEqual(['a', 'c']);
	});
});
