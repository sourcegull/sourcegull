import { describe, expect, it } from 'vitest';
import {
	isExclusive,
	kindLabel,
	managedTitle,
	managesWholeKind,
	sourceVariable,
	type DeclaredKindsFile
} from './declared-config.ts';

const resources = (exclusive: boolean, kinds: string[]): DeclaredKindsFile => ({
	source: 'resources',
	kinds,
	exclusive
});

describe('declared configuration', () => {
	it('is exclusive only for the kinds an exclusive resources file lists', () => {
		const files = [resources(true, ['connections', 'roles'])];
		expect(isExclusive(files, 'connections')).toBe(true);
		expect(isExclusive(files, 'roles')).toBe(true);
		expect(isExclusive(files, 'featureFlags')).toBe(false);
		expect(isExclusive([resources(false, ['connections'])], 'connections')).toBe(false);
		expect(isExclusive([{ source: 'global_settings', kinds: ['connections'], exclusive: true }], 'connections')).toBe(
			false
		);
		expect(isExclusive([], 'connections')).toBe(false);
		expect(isExclusive(undefined, 'connections')).toBe(false);
	});

	it('tells an entry from a whole kind', () => {
		expect(managesWholeKind('connections[gitlab-main]')).toBe(false);
		expect(managesWholeKind('connections')).toBe(true);
		expect(managedTitle('roles[auditor]')).toBe(
			'Managed by SOURCEGULL_RESOURCES_FILE (roles[auditor]); change it in the file.'
		);
	});

	it('names sources and kinds', () => {
		expect(sourceVariable('resources')).toBe('SOURCEGULL_RESOURCES_FILE');
		expect(sourceVariable('global_settings')).toBe('SOURCEGULL_GLOBAL_SETTINGS_FILE');
		expect(kindLabel('executorSecrets')).toBe('global executor secrets');
		expect(kindLabel('somethingNew')).toBe('somethingNew');
	});
});
