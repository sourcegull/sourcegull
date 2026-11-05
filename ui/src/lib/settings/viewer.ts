/**
 * Reads what the UI acts on from the viewer's merged settings (`viewerSettings.final`). The server
 * checks each document when it is saved, but a merged value can still be of an older shape, so
 * anything unexpected counts as unset.
 */
import { safeHref } from '#lib/batches/markdown.ts';
import { parsePatternType, type PatternType } from '#lib/search/query.ts';
import { readStorage, writeStorage } from '#lib/theme/theme.ts';

export interface Quicklink {
	name: string;
	url: string;
	description: string | null;
}

export type NoticeLocation = 'top' | 'home';

export interface Notice {
	message: string;
	location: NoticeLocation;
	dismissible: boolean;
}

function parseObject(final: string): Record<string, unknown> {
	try {
		const value: unknown = JSON.parse(final);
		return value && typeof value === 'object' && !Array.isArray(value) ? (value as Record<string, unknown>) : {};
	} catch {
		return {};
	}
}

function objects(value: unknown): Record<string, unknown>[] {
	return Array.isArray(value) ? value.filter((v) => v && typeof v === 'object' && !Array.isArray(v)) : [];
}

/** `search.defaultPatternType`: the pattern type the search boxes start with. */
export function defaultPatternType(final: string): PatternType {
	const value = parseObject(final)['search.defaultPatternType'];
	return parsePatternType(typeof value === 'string' ? value : null);
}

/** `quicklinks`, without those that have no name or whose target is not a safe link. */
export function quicklinks(final: string): Quicklink[] {
	return objects(parseObject(final).quicklinks).flatMap((q) => {
		const url = typeof q.url === 'string' ? safeHref(q.url) : null;
		if (!url || typeof q.name !== 'string' || !q.name.trim()) return [];
		return [{ name: q.name, url, description: typeof q.description === 'string' ? q.description : null }];
	});
}

/** `notices` shown at `location`, without empty ones. */
export function notices(final: string, location: NoticeLocation): Notice[] {
	return objects(parseObject(final).notices).flatMap((n) => {
		if (n.location !== location || typeof n.message !== 'string' || !n.message.trim()) return [];
		return [{ message: n.message, location, dismissible: n.dismissible === true }];
	});
}

/**
 * Merges settings documents in cascade order, as the server does (`merge_values` in
 * `crates/config`): objects merge key by key, anything else replaces the earlier value.
 */
export function mergeSettings(documents: (Record<string, unknown> | null)[]): Record<string, unknown> {
	const merge = (base: unknown, over: unknown): unknown => {
		if (!isPlainObject(base) || !isPlainObject(over)) return over;
		const out: Record<string, unknown> = { ...base };
		for (const [k, v] of Object.entries(over)) out[k] = k in out ? merge(out[k], v) : v;
		return out;
	};
	return documents.reduce<Record<string, unknown>>(
		(acc, doc) => (doc ? (merge(acc, doc) as Record<string, unknown>) : acc),
		{}
	);
}

function isPlainObject(v: unknown): v is Record<string, unknown> {
	return !!v && typeof v === 'object' && !Array.isArray(v);
}

const DISMISSED_KEY = 'sourcegull.dismissedNotices';

/** The messages of the notices this browser dismissed. A changed message shows again. */
export function loadDismissedNotices(): string[] {
	try {
		const value: unknown = JSON.parse(readStorage(DISMISSED_KEY) ?? '[]');
		return Array.isArray(value) ? value.filter((v): v is string => typeof v === 'string') : [];
	} catch {
		return [];
	}
}

export function storeDismissedNotices(messages: string[]): void {
	writeStorage(DISMISSED_KEY, messages.length ? JSON.stringify(messages) : null);
}
