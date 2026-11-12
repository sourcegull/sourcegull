import { afterEach, describe, expect, it } from 'vitest';
import {
	applyDisplaySettings,
	formatDate,
	formatDateTime,
	formatNumber,
	parseDisplayPreferences
} from './format.svelte.ts';

const final = (value: unknown) => JSON.stringify(value);
const local = new Date(2026, 9, 30, 14, 5, 9);

afterEach(() => applyDisplaySettings('{}'));

describe('parseDisplayPreferences', () => {
	it('reads the settings, and treats anything unknown as unset', () => {
		expect(parseDisplayPreferences(final({ 'display.dateFormat': 'iso', 'display.locale': 'de-DE' }))).toEqual({
			dateFormat: 'iso',
			locale: 'de-DE'
		});
		expect(parseDisplayPreferences(final({ 'display.dateFormat': 'us', 'display.locale': 'not a locale' }))).toEqual({
			dateFormat: 'locale',
			locale: undefined
		});
		expect(parseDisplayPreferences(final({ 'display.locale': 3 })).locale).toBeUndefined();
		expect(parseDisplayPreferences('not json')).toEqual({ dateFormat: 'locale', locale: undefined });
	});
});

describe('locale format', () => {
	it('follows display.locale', () => {
		applyDisplaySettings(final({ 'display.locale': 'de-DE' }));
		expect(formatDate(local)).toBe('30.10.2026');
		expect(formatDateTime(local)).toBe('30.10.2026, 14:05:09');
		expect(formatNumber(1234567)).toBe('1.234.567');
		applyDisplaySettings(final({ 'display.locale': 'en-GB' }));
		expect(formatDate(local)).toBe('30/10/2026');
	});

	it('passes the options through', () => {
		applyDisplaySettings(final({ 'display.locale': 'en-US' }));
		expect(formatDate('2026-10-19T00:00:00Z', { month: 'short', day: 'numeric', timeZone: 'UTC' })).toBe('Oct 19');
	});

	it('gives the fallback for no or a bad date', () => {
		expect(formatDateTime(null, undefined, '—')).toBe('—');
		expect(formatDate('', undefined, 'never')).toBe('never');
		expect(formatDate('garbage')).toBe('');
	});
});

describe('ISO format', () => {
	it('is the same in every locale', () => {
		for (const locale of ['en-US', 'de-DE']) {
			applyDisplaySettings(final({ 'display.dateFormat': 'iso', 'display.locale': locale }));
			expect(formatDate(local)).toBe('2026-10-30');
			expect(formatDateTime(local)).toBe('2026-10-30 14:05:09');
		}
	});

	it('drops seconds for short times, the year when asked, and honours UTC', () => {
		applyDisplaySettings(final({ 'display.dateFormat': 'iso' }));
		expect(formatDateTime(local, { dateStyle: 'medium', timeStyle: 'short' })).toBe('2026-10-30 14:05');
		expect(formatDate(local, { year: 'numeric', month: 'short', day: 'numeric' })).toBe('2026-10-30');
		expect(formatDate('2026-10-19T00:00:00Z', { month: 'short', day: 'numeric', timeZone: 'UTC' })).toBe('10-19');
	});
});
