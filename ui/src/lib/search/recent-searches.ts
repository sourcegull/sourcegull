/**
 * Recent searches, kept in the `search.input.recentSearches` temporary setting: the newest searches first, one entry per query, at most {@link MAX_RECENT_SEARCHES},
 * each with its result count and time. Signed-in users keep them on the server (GraphQL
 * `temporarySettings`); anonymous visitors in local storage.
 */
import { writable, type Readable } from 'svelte/store';
import { graphql } from '#lib/graphql.ts';
import type { TemporarySettingsQuery } from '#lib/graphql-types.ts';

/** The temporary settings key. */
export const RECENT_SEARCHES_KEY = 'search.input.recentSearches';
/** How many recent searches are kept. */
export const MAX_RECENT_SEARCHES = 20;
/** Local storage key of the anonymous visitor's temporary settings. */
export const LOCAL_STORAGE_KEY = 'temporarySettings';

export interface RecentSearch {
	/** The query as submitted, with `context:` and a non-default `patternType:`. */
	query: string;
	resultCount: number;
	/** Whether the result count was cut off by a limit (shown as `500+`). */
	limitHit: boolean;
	/** ISO 8601. */
	timestamp: string;
}

/** Valid entries of a stored value (anything else is dropped), newest first, capped. */
export function parseRecentSearches(raw: unknown): RecentSearch[] {
	if (!Array.isArray(raw)) return [];
	const out: RecentSearch[] = [];
	for (const e of raw) {
		if (!e || typeof e !== 'object') continue;
		const r = e as Record<string, unknown>;
		if (typeof r.query !== 'string' || !r.query.trim() || typeof r.timestamp !== 'string') continue;
		if (out.some((o) => o.query === r.query)) continue;
		out.push({
			query: r.query,
			resultCount: typeof r.resultCount === 'number' ? r.resultCount : 0,
			limitHit: r.limitHit === true,
			timestamp: r.timestamp
		});
	}
	return out.slice(0, MAX_RECENT_SEARCHES);
}

/** `list` with `entry` first and any older entry of the same query removed, capped. */
export function addRecentSearch(list: RecentSearch[], entry: RecentSearch): RecentSearch[] {
	const query = entry.query.trim();
	if (!query) return list;
	return [{ ...entry, query }, ...list.filter((e) => e.query !== query)].slice(0, MAX_RECENT_SEARCHES);
}

/** `list` without `query`. */
export function removeRecentSearch(list: RecentSearch[], query: string): RecentSearch[] {
	return list.filter((e) => e.query !== query);
}

/**
 * The entries to suggest for what is typed: all of them for an empty input, otherwise those
 * containing the typed text (ignoring case) other than the text itself.
 */
export function matchRecentSearches(list: RecentSearch[], input: string, limit = 10): RecentSearch[] {
	const q = input.trim().toLowerCase();
	if (!q) return list.slice(0, limit);
	return list.filter((e) => e.query.toLowerCase() !== q && e.query.toLowerCase().includes(q)).slice(0, limit);
}

/** `12 results`, `500+ results`, `1 result`. */
export function resultCountLabel(e: Pick<RecentSearch, 'resultCount' | 'limitHit'>): string {
	return `${e.resultCount}${e.limitHit ? '+' : ''} result${e.resultCount === 1 && !e.limitHit ? '' : 's'}`;
}

/** `just now`, `5 minutes ago`, `3 hours ago`, `2 days ago`, then the date. */
export function relativeTime(timestamp: string, now: Date = new Date()): string {
	const t = new Date(timestamp);
	if (Number.isNaN(t.getTime())) return '';
	const s = Math.max(0, Math.round((now.getTime() - t.getTime()) / 1000));
	const ago = (n: number, unit: string) => `${n} ${unit}${n === 1 ? '' : 's'} ago`;
	if (s < 60) return 'just now';
	if (s < 3600) return ago(Math.floor(s / 60), 'minute');
	if (s < 86400) return ago(Math.floor(s / 3600), 'hour');
	if (s < 30 * 86400) return ago(Math.floor(s / 86400), 'day');
	return t.toISOString().slice(0, 10);
}

/** Where recent searches are kept. */
export interface RecentSearchesBackend {
	load(): Promise<RecentSearch[]>;
	save(list: RecentSearch[]): Promise<void>;
}

export const TEMPORARY_SETTINGS = `query TemporarySettings { temporarySettings { contents } }`;
export const EDIT_TEMPORARY_SETTINGS = `mutation EditTemporarySettings($contents: String!) {
	editTemporarySettings(settingsToEdit: $contents) { alwaysNil }
}`;

/** The signed-in user's temporary settings on the server. */
export const serverBackend: RecentSearchesBackend = {
	async load() {
		const r = await graphql<TemporarySettingsQuery>(TEMPORARY_SETTINGS);
		const all = JSON.parse(r.temporarySettings.contents) as Record<string, unknown>;
		return parseRecentSearches(all[RECENT_SEARCHES_KEY]);
	},
	async save(list) {
		await graphql(EDIT_TEMPORARY_SETTINGS, {
			contents: JSON.stringify({ [RECENT_SEARCHES_KEY]: list })
		});
	}
};

/** Local storage; storage that is missing, full or blocked reads as empty and drops writes. */
export function localBackend(storage: () => Storage | undefined = () => globalThis.localStorage): RecentSearchesBackend {
	const readAll = (): Record<string, unknown> => {
		try {
			const raw = storage()?.getItem(LOCAL_STORAGE_KEY);
			const v: unknown = raw ? JSON.parse(raw) : {};
			return v && typeof v === 'object' && !Array.isArray(v) ? (v as Record<string, unknown>) : {};
		} catch {
			return {};
		}
	};
	return {
		async load() {
			return parseRecentSearches(readAll()[RECENT_SEARCHES_KEY]);
		},
		async save(list) {
			try {
				storage()?.setItem(LOCAL_STORAGE_KEY, JSON.stringify({ ...readAll(), [RECENT_SEARCHES_KEY]: list }));
			} catch {
				// Storage unavailable: recent searches live for this page only.
			}
		}
	};
}

export interface RecentSearches extends Readable<RecentSearch[]> {
	/** Loads the stored list (once; later calls return the same promise). */
	load(): Promise<void>;
	add(entry: RecentSearch): Promise<void>;
	remove(query: string): Promise<void>;
	clear(): Promise<void>;
}

/** A store of recent searches kept in `backend`. Write failures keep the in-memory list. */
export function createRecentSearches(backend: RecentSearchesBackend): RecentSearches {
	const { subscribe, set, update } = writable<RecentSearch[]>([]);
	let current: RecentSearch[] = [];
	subscribe((v) => (current = v));
	let loading: Promise<void> | undefined;
	const load = () =>
		(loading ??= backend
			.load()
			// Entries added before the load finished come first.
			.then((stored) => {
				let merged = stored;
				for (const e of [...current].reverse()) merged = addRecentSearch(merged, e);
				set(merged);
			})
			.catch(() => {}));
	const change = async (f: (l: RecentSearch[]) => RecentSearch[]) => {
		await load();
		update(f);
		await backend.save(current).catch(() => {});
	};
	return {
		subscribe,
		load,
		add: (entry) => change((l) => addRecentSearch(l, entry)),
		remove: (query) => change((l) => removeRecentSearch(l, query)),
		clear: () => change(() => [])
	};
}

let shared: { user: string | null; store: RecentSearches } | undefined;

/**
 * The page-wide store for the signed-in user `user` (on the server) or, for `null`, the
 * anonymous visitor (in local storage).
 */
export function recentSearches(user: string | null): RecentSearches {
	if (!shared || shared.user !== user) {
		shared = { user, store: createRecentSearches(user ? serverBackend : localBackend()) };
	}
	return shared.store;
}

/**
 * The highlighted row after moving by `delta` in a list of `n` rows, wrapping around; `-1`
 * (none highlighted) moves to the first row going down and the last going up.
 */
export function stepIndex(active: number, n: number, delta: 1 | -1): number {
	if (n <= 0) return -1;
	if (active < 0) return delta > 0 ? 0 : n - 1;
	return (active + delta + n) % n;
}

/**
 * Shell-like history recall (Ctrl/⌘+↑ older, Ctrl/⌘+↓ newer): the next position in `list`
 * from `index` (`-1` = the text being typed), or `null` past either end.
 */
export function recallIndex(index: number, n: number, older: boolean): number | null {
	const next = index + (older ? 1 : -1);
	if (next < -1 || next >= n) return null;
	return next;
}

/** The query for a recent search entry: `context:` kept, a non-default pattern type appended. */
export function recentQuery(query: string, patternType: string): string {
	const q = query.trim();
	return patternType === 'keyword' || /(^|\s)patterntype:/i.test(q) ? q : `${q} patternType:${patternType}`;
}

/** Splits a leading `context:…` filter (and its space) off a stored query, so the dropdown can dim it. */
export function splitContext(query: string): { context: string; rest: string } {
	const m = /^context:\S+\s+/.exec(query);
	return m ? { context: m[0], rest: query.slice(m[0].length) } : { context: '', rest: query };
}
