import { describe, expect, it } from 'vitest';
import { csvURL, isActive, progressPercent } from './search-jobs.ts';

describe('search jobs', () => {
	it('derives the CSV download from the JSONL one', () => {
		expect(csvURL('https://s.example.com/.api/search/export/7.jsonl')).toBe('https://s.example.com/.api/search/export/7.csv');
	});
	it('tells running jobs apart', () => {
		expect(isActive('QUEUED')).toBe(true);
		expect(isActive('PROCESSING')).toBe(true);
		expect(isActive('COMPLETED')).toBe(false);
		expect(isActive('CANCELED')).toBe(false);
	});
	it('counts failed revisions as done', () => {
		expect(progressPercent({ total: 0, completed: 0, failed: 0, inProgress: 0 })).toBe(0);
		expect(progressPercent({ total: 4, completed: 2, failed: 1, inProgress: 1 })).toBe(75);
	});
});
