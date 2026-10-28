import { existsSync, readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import { EVENTS } from './events.ts';

const namesPath = new URL('../../../crates/analytics/src/names.rs', import.meta.url);

describe.skipIf(!existsSync(namesPath))('usage event names', () => {
	it('match the names the admin analytics count', () => {
		const names = readFileSync(namesPath, 'utf8');
		for (const name of Object.values(EVENTS)) {
			expect(names, name).toContain(`: &str = "${name}";`);
		}
	});
});
