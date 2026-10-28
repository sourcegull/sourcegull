import { describe, expect, it } from 'vitest';
import { viewBlobArgument } from '#lib/events.ts';
import {
	canAssign,
	coverage,
	isSignal,
	ownerName,
	parsePatterns,
	reasonLabel,
	type OwnerPerson,
	type OwnerTeam
} from './queries.ts';

describe('ownership helpers', () => {
	it('labels reasons', () => {
		const file = { __typename: 'GitBlob' as const, path: '.gitlab/CODEOWNERS', url: '/r/-/blob/.gitlab/CODEOWNERS' };
		expect(
			reasonLabel({ __typename: 'CodeownersFileEntry', ruleLineMatch: 4, section: 'codeowners', pattern: '*', codeownersFile: file })
		).toBe('CODEOWNERS line 4');
		expect(
			reasonLabel({ __typename: 'CodeownersFileEntry', ruleLineMatch: 7, section: 'Docs', pattern: '/docs/', codeownersFile: file })
		).toBe('CODEOWNERS line 7 [Docs]');
		expect(reasonLabel({ __typename: 'AssignedOwner', isDirectMatch: false })).toBe('Assigned to a parent directory');
		const commits = { __typename: 'RecentContributorOwnershipSignal' as const, contributionCount: 3 };
		expect(reasonLabel(commits)).toBe('Recent contributor · 3 commits');
		expect(reasonLabel({ __typename: 'RecentViewOwnershipSignal', viewCount: 1 })).toBe('Recent viewer · 1 view');
		expect(isSignal(commits)).toBe(true);
		expect(isSignal({ __typename: 'AssignedOwner', isDirectMatch: true })).toBe(false);
	});

	it('computes coverage and parses patterns', () => {
		expect(coverage(1, 3)).toBe(33.3);
		expect(coverage(0, 0)).toBeNull();
		expect(parsePatterns(' a/*\n\nb, c ')).toEqual(['a/*', 'b', 'c']);
		expect(viewBlobArgument('r', '/a/b.go')).toEqual({ repoName: 'r', filePath: 'a/b.go' });
	});

	it('names owners', () => {
		const person: OwnerPerson = {
			__typename: 'Person',
			name: 'ali-gl',
			email: '',
			displayName: 'ali-gl',
			avatarURL: null,
			user: null
		};
		expect(ownerName(person)).toBe('@ali-gl');
		expect(ownerName({ ...person, user: { id: 'u', username: 'alice', displayName: null, url: '/users/alice' } })).toBe(
			'alice'
		);
		expect(ownerName({ ...person, name: '', email: 'a@example.com' })).toBe('a@example.com');
		// A commit author: name and email.
		expect(ownerName({ ...person, name: 'Bob B', displayName: 'Bob B', email: 'bob@example.com' })).toBe('Bob B');
		const team: OwnerTeam = { __typename: 'Team', id: 't', name: 'acme/backend', teamDisplayName: null, url: '', external: true };
		expect(ownerName(team)).toBe('acme/backend');
	});

	it('checks OWNERSHIP#ASSIGN', () => {
		expect(canAssign(null)).toBe(false);
		expect(canAssign({ id: 'u', permissions: { nodes: [{ namespace: 'BATCH_CHANGES', action: 'READ' }] } })).toBe(false);
		expect(canAssign({ id: 'u', permissions: { nodes: [{ namespace: 'OWNERSHIP', action: 'ASSIGN' }] } })).toBe(true);
	});
});
