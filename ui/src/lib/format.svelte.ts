/**
 * Dates, times and numbers as the UI shows them, following the viewer's `display.dateFormat` and
 * `display.locale` settings. Without them the browser's language decides. The preferences are
 * reactive state, so what a page shows updates once the settings have loaded.
 */

export type DateFormat = 'locale' | 'iso';

export interface DisplayPreferences {
	dateFormat: DateFormat;
	/** A locale the browser supports, or `undefined` for the browser's own. */
	locale: string | undefined;
}

const preferences = $state<DisplayPreferences>({ dateFormat: 'locale', locale: undefined });

/** Reads the display preferences from the viewer's merged settings (`viewerSettings.final`). */
export function parseDisplayPreferences(final: string): DisplayPreferences {
	let doc: Record<string, unknown> = {};
	try {
		const value: unknown = JSON.parse(final);
		if (value && typeof value === 'object' && !Array.isArray(value)) doc = value as Record<string, unknown>;
	} catch {
		// Unset.
	}
	return {
		dateFormat: doc['display.dateFormat'] === 'iso' ? 'iso' : 'locale',
		locale: supportedLocale(doc['display.locale'])
	};
}

function supportedLocale(value: unknown): string | undefined {
	if (typeof value !== 'string' || !value.trim()) return undefined;
	try {
		return Intl.DateTimeFormat.supportedLocalesOf([value.trim()])[0];
	} catch {
		return undefined;
	}
}

/** Applies the display preferences of the viewer's merged settings. */
export function applyDisplaySettings(final: string): void {
	const next = parseDisplayPreferences(final);
	preferences.dateFormat = next.dateFormat;
	preferences.locale = next.locale;
}

type DateInput = string | number | Date;

function toDate(value: DateInput): Date | null {
	const d = value instanceof Date ? value : new Date(value);
	return Number.isNaN(d.getTime()) ? null : d;
}

const pad = (n: number) => String(n).padStart(2, '0');

/** `YYYY-MM-DD`, or `MM-DD` when `options` leave the year out; in UTC when they ask for it. */
function isoDate(d: Date, options?: Intl.DateTimeFormatOptions): string {
	const utc = options?.timeZone === 'UTC';
	const year = utc ? d.getUTCFullYear() : d.getFullYear();
	const month = pad((utc ? d.getUTCMonth() : d.getMonth()) + 1);
	const day = pad(utc ? d.getUTCDate() : d.getDate());
	const withoutYear = options && (options.month || options.day) && !options.year && !options.dateStyle;
	return withoutYear ? `${month}-${day}` : `${year}-${month}-${day}`;
}

/** `HH:MM:SS`, or `HH:MM` when `options` ask for short times. */
function isoTime(d: Date, options?: Intl.DateTimeFormatOptions): string {
	const utc = options?.timeZone === 'UTC';
	const hm = `${pad(utc ? d.getUTCHours() : d.getHours())}:${pad(utc ? d.getUTCMinutes() : d.getMinutes())}`;
	const seconds =
		!options || options.second !== undefined || (options.timeStyle !== undefined && options.timeStyle !== 'short');
	return seconds ? `${hm}:${pad(utc ? d.getUTCSeconds() : d.getSeconds())}` : hm;
}

/** A date and time, like `Date.toLocaleString(locale, options)`. `fallback` for no or a bad date. */
export function formatDateTime(
	value: DateInput | null | undefined,
	options?: Intl.DateTimeFormatOptions,
	fallback = ''
): string {
	const d = value == null || value === '' ? null : toDate(value);
	if (!d) return fallback;
	if (preferences.dateFormat === 'iso') return `${isoDate(d, options)} ${isoTime(d, options)}`;
	return d.toLocaleString(preferences.locale, options);
}

/** A date without the time, like `Date.toLocaleDateString(locale, options)`. */
export function formatDate(
	value: DateInput | null | undefined,
	options?: Intl.DateTimeFormatOptions,
	fallback = ''
): string {
	const d = value == null || value === '' ? null : toDate(value);
	if (!d) return fallback;
	if (preferences.dateFormat === 'iso') return isoDate(d, options);
	return d.toLocaleDateString(preferences.locale, options);
}

/** A number with the locale's digit grouping, like `Number.toLocaleString(locale)`. */
export function formatNumber(value: number, options?: Intl.NumberFormatOptions): string {
	return value.toLocaleString(preferences.locale, options);
}
