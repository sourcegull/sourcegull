import { describe, expect, it } from 'vitest';
import { allStatements, sourcegullVersion } from './queries.ts';

describe('updates helpers', () => {
	it('extracts the Sourcegull version', () => {
		expect(sourcegullVersion('5.3.0+sourcegull.0.1.0')).toBe('0.1.0');
		expect(sourcegullVersion('0.2.0')).toBe('0.2.0');
	});

	it('joins the fix statements', () => {
		const d = (statements: string[] | null) => ({
			name: 'x',
			problem: 'missing',
			solution: '',
			diff: null,
			statements,
			urlHint: null
		});
		expect(allStatements([d(['A;', 'B;']), d(null), d(['C;'])])).toBe('A;\nB;\nC;');
		expect(allStatements([])).toBe('');
	});
});
