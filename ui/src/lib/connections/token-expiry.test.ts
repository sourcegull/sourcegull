import { describe, expect, it } from 'vitest';
import { daysLeft, expiryLevel, expiryText } from './token-expiry.ts';

const now = Date.parse('2026-11-12T09:00:00Z');
const at = (iso: string) => ({ expiresAt: iso, neverExpires: false });

describe('token expiry', () => {
	it('counts whole days, rounded up', () => {
		expect(daysLeft('2026-11-22T00:00:00Z', now)).toBe(10);
		expect(daysLeft('2026-11-13T00:00:00Z', now)).toBe(1);
		expect(daysLeft('2026-11-01T00:00:00Z', now)).toBe(0);
	});

	it('describes the expiry', () => {
		expect(expiryText(at('2026-11-22T00:00:00Z'), now)).toBe('expires 2026-11-22 (in 10 days)');
		expect(expiryText(at('2026-11-13T00:00:00Z'), now)).toBe('expires 2026-11-13 (tomorrow)');
		expect(expiryText(at('2026-11-01T00:00:00Z'), now)).toBe('expired 2026-11-01');
		expect(expiryText({ expiresAt: null, neverExpires: true }, now)).toBe('no expiry date');
	});

	it('flags tokens that expire within 14 days', () => {
		expect(expiryLevel(at('2026-12-31T00:00:00Z'), now)).toBe('ok');
		expect(expiryLevel(at('2026-11-22T00:00:00Z'), now)).toBe('soon');
		expect(expiryLevel(at('2026-11-01T00:00:00Z'), now)).toBe('expired');
		expect(expiryLevel({ expiresAt: null, neverExpires: true }, now)).toBe('ok');
		expect(expiryLevel(null, now)).toBe('unknown');
	});
});
