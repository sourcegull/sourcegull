import { describe, expect, it } from 'vitest';
import { allStatements } from './queries.ts';

describe('updates helpers', () => {
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
