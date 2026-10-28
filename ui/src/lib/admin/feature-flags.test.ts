import { describe, expect, it } from 'vitest';
import { describeFlag, flagValueVariables, namespaceName } from './feature-flags.ts';

describe('feature flag helpers', () => {
	it('builds value variables', () => {
		expect(flagValueVariables('bool', true, 0)).toEqual({
			value: true,
			rolloutBasisPoints: null
		});
		expect(flagValueVariables('rollout', false, 12.5)).toEqual({
			value: null,
			rolloutBasisPoints: 1250
		});
		expect(flagValueVariables('rollout', false, 150)).toEqual({
			value: null,
			rolloutBasisPoints: 10000
		});
		expect(flagValueVariables('rollout', false, -1)).toEqual({
			value: null,
			rolloutBasisPoints: 0
		});
	});

	it('describes flags and overrides', () => {
		const base = { name: 'f', createdAt: '', updatedAt: '', managedBy: null, overrides: [] };
		expect(describeFlag({ ...base, __typename: 'FeatureFlagBoolean', value: true })).toBe('on');
		expect(
			describeFlag({
				...base,
				__typename: 'FeatureFlagRollout',
				rolloutBasisPoints: 2500
			})
		).toBe('25% of users');
		expect(
			namespaceName({
				id: '1',
				value: true,
				namespace: { __typename: 'Org', id: 'o', name: 'acme' }
			})
		).toBe('acme');
	});
});
