import { describe, expect, it } from 'vitest';
import { formatFinalSettings, isEmailAddress, parseUsernames, teamUrl } from './queries.ts';

describe('organization and team helpers', () => {
	it('tells email addresses from usernames', () => {
		expect(isEmailAddress('alice')).toBe(false);
		expect(isEmailAddress(' dave@example.com ')).toBe(true);
		expect(isEmailAddress('a b@example.com')).toBe(false);
		expect(isEmailAddress('@alice')).toBe(false);
	});

	it('builds team URLs with path segments', () => {
		expect(teamUrl('acme')).toBe('/teams/acme');
		expect(teamUrl('acme/backend db')).toBe('/teams/acme/backend%20db');
	});

	it('parses lists of usernames', () => {
		expect(parseUsernames('alice, @bob\ncarol  alice')).toEqual(['alice', 'bob', 'carol']);
		expect(parseUsernames(' , ')).toEqual([]);
	});

	it('pretty-prints merged settings', () => {
		expect(formatFinalSettings('{"a":1}')).toBe('{\n  "a": 1\n}');
		expect(formatFinalSettings('not json')).toBe('not json');
	});
});
