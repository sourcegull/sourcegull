/**
 * The initial values of the trend creation wizards, as the
 * original's: a wizard saves its fields as a draft in local storage while you edit, restores
 * them when you come back, and clears them on submit or cancel. The search and capture group
 * wizards also take values from URL parameters, for the one-click "Create trend" from the
 * search results page (`?query=`) and for links that encode a trend (`repoQuery`,
 * `repositories`, `title`, `series` or `groupSearchQuery`). URL values win over the draft.
 */
import {
	defaultCaptureGroupFields,
	defaultComputeFields,
	defaultLangStatsFields,
	defaultSearchFields,
	GROUP_BY_OPTIONS,
	newSeries,
	STEP_UNITS,
	tokens,
	type CaptureGroupFields,
	type ComputeFields,
	type EditableSeries,
	type LangStatsFields,
	type RepoFields,
	type RepoMode,
	type SearchInsightFields
} from './creation.ts';
import type { InsightType } from './insights.ts';

/** The fields of each wizard. */
export interface CreationFields {
	search: SearchInsightFields;
	'capture-group': CaptureGroupFields;
	compute: ComputeFields;
	'language-stats': LangStatsFields;
}

/** The local storage keys of the drafts. */
export const DRAFT_KEYS: Record<InsightType, string> = {
	search: 'insights.search-insight-creation-ui-v2',
	'capture-group': 'insights.capture-group-creation-ui-v2',
	compute: 'insights.compute-creation-ui-v2',
	'language-stats': 'insights.code-stats-creation-ui'
};

type DraftStorage = Pick<Storage, 'getItem' | 'setItem' | 'removeItem'>;
/** URL parameters (SvelteKit's `page.url.searchParams` is read-only). */
type Params = Pick<URLSearchParams, 'get'>;

/** The browser's local storage, or null where it is unavailable (blocked, private mode). */
function browserStorage(): DraftStorage | null {
	try {
		return globalThis.localStorage ?? null;
	} catch {
		return null;
	}
}

// ---- Sanitizing ------------------------------------------------------------------------------------

type Raw = Record<string, unknown>;

const isObject = (v: unknown): v is Raw => typeof v === 'object' && v !== null && !Array.isArray(v);
const str = (v: unknown, fallback: string): string => (typeof v === 'string' ? v : fallback);
const strings = (v: unknown, fallback: string[]): string[] =>
	Array.isArray(v) ? v.filter((s): s is string => typeof s === 'string') : fallback;
const oneOf = <T extends string>(v: unknown, values: readonly T[], fallback: T): T =>
	values.includes(v as T) ? (v as T) : fallback;

const REPO_MODES: RepoMode[] = ['search-query', 'urls-list', 'all-repos'];

/** Series of a draft or a URL (`{ name, query, stroke }` each), with fresh form keys. */
function seriesOf(v: unknown, fallback: EditableSeries[]): EditableSeries[] {
	if (!Array.isArray(v)) return fallback;
	const out = v.filter(isObject).map((s, i) => {
		const init: Partial<EditableSeries> = { name: str(s.name, ''), query: str(s.query, '') };
		if (typeof s.stroke === 'string' && s.stroke !== '') init.stroke = s.stroke;
		return newSeries(i, init);
	});
	return out.length > 0 ? out : fallback;
}

function repoFieldsOf(raw: Raw, d: RepoFields): RepoFields {
	return {
		repoMode: oneOf(raw.repoMode, REPO_MODES, d.repoMode),
		repoQuery: str(raw.repoQuery, d.repoQuery),
		repositories: strings(raw.repositories, d.repositories)
	};
}

const STEPS = STEP_UNITS.map((s) => s.unit);
const GROUP_BYS = GROUP_BY_OPTIONS.map((o) => o.value);

/** The fields of a stored draft, over the defaults; anything malformed falls back to them. */
export function sanitizeDraft<T extends InsightType>(type: T, raw: unknown): CreationFields[T] | null {
	if (!isObject(raw)) return null;
	switch (type) {
		case 'search': {
			const d = defaultSearchFields();
			const f: SearchInsightFields = {
				...repoFieldsOf(raw, d),
				title: str(raw.title, d.title),
				series: seriesOf(raw.series, d.series),
				stepUnit: oneOf(raw.stepUnit, STEPS, d.stepUnit),
				stepValue: str(raw.stepValue, d.stepValue)
			};
			return f as CreationFields[T];
		}
		case 'capture-group': {
			const d = defaultCaptureGroupFields();
			const f: CaptureGroupFields = {
				...repoFieldsOf(raw, d),
				title: str(raw.title, d.title),
				query: str(raw.query, d.query),
				stepUnit: oneOf(raw.stepUnit, STEPS, d.stepUnit),
				stepValue: str(raw.stepValue, d.stepValue)
			};
			return f as CreationFields[T];
		}
		case 'compute': {
			const d = defaultComputeFields();
			const f: ComputeFields = {
				title: str(raw.title, d.title),
				repositories: strings(raw.repositories, d.repositories),
				series: seriesOf(raw.series, d.series),
				groupBy: oneOf(raw.groupBy, GROUP_BYS, d.groupBy)
			};
			return f as CreationFields[T];
		}
		default: {
			const d = defaultLangStatsFields();
			const f: LangStatsFields = {
				title: str(raw.title, d.title),
				repository: str(raw.repository, d.repository),
				threshold: str(raw.threshold, d.threshold)
			};
			return f as CreationFields[T];
		}
	}
}

// ---- Drafts ------------------------------------------------------------------------------------------

/** The saved draft of a wizard, or null (none, unreadable, or storage unavailable). */
export function loadDraft<T extends InsightType>(
	type: T,
	storage: DraftStorage | null = browserStorage()
): CreationFields[T] | null {
	try {
		const raw = storage?.getItem(DRAFT_KEYS[type]);
		return raw ? sanitizeDraft(type, JSON.parse(raw)) : null;
	} catch {
		return null;
	}
}

/** Saves a wizard's fields (the form keys and server ids of series left out). */
export function saveDraft<T extends InsightType>(
	type: T,
	fields: CreationFields[T],
	storage: DraftStorage | null = browserStorage()
): void {
	const value =
		'series' in fields
			? { ...fields, series: fields.series.map((s) => ({ name: s.name, query: s.query, stroke: s.stroke })) }
			: fields;
	try {
		storage?.setItem(DRAFT_KEYS[type], JSON.stringify(value));
	} catch {
		// Full or blocked storage: the draft is a convenience.
	}
}

export function clearDraft(type: InsightType, storage: DraftStorage | null = browserStorage()): void {
	try {
		storage?.removeItem(DRAFT_KEYS[type]);
	} catch {
		// Blocked storage: nothing was saved either.
	}
}

// ---- URL parameters ------------------------------------------------------------------------------

/**
 * A search query split for the search wizard: the `repo:` filters go to the
 * repository query, the rest (without `repo:` and `context:` filters) is the series query.
 */
export function insightDataFromQuery(query: string): { repoQuery: string; seriesQuery: string } {
	const repo: string[] = [];
	const rest: string[] = [];
	for (const t of tokens(query)) {
		const field = /^-?([A-Za-z]+):/.exec(t)?.[1]?.toLowerCase();
		if (field === 'repo' || field === 'r') repo.push(t);
		else if (field !== 'context') rest.push(t);
	}
	return { repoQuery: repo.join(' '), seriesQuery: rest.join(' ') };
}

/**
 * A parameter value; links encode values once more (`encodeURIComponent`) before
 * `URLSearchParams` does, so a value is decoded again where that is possible.
 */
function param(params: Params, name: string): string | null {
	const v = params.get(name);
	if (v === null) return null;
	try {
		return decodeURIComponent(v);
	} catch {
		return v;
	}
}

/** The search wizard's values from the URL: `query` first, else an encoded trend; or null. */
export function searchFieldsFromURL(params: Params): SearchInsightFields | null {
	const d = defaultSearchFields();
	const query = params.get('query');
	if (query) {
		const { repoQuery, seriesQuery } = insightDataFromQuery(query);
		return {
			...d,
			repoMode: repoQuery ? 'search-query' : 'urls-list',
			repoQuery,
			series: [newSeries(0, { name: 'Search series #1', query: seriesQuery })]
		};
	}
	const repoQuery = param(params, 'repoQuery');
	const repositories = params.get('repositories');
	const title = param(params, 'title');
	let series: EditableSeries[] = [];
	try {
		series = seriesOf(JSON.parse(param(params, 'series') ?? '[]'), []);
	} catch {
		series = [];
	}
	if (!repoQuery && !repositories && !title && series.length === 0) return null;
	return {
		...d,
		title: title ?? '',
		repoMode: repoQuery ? 'search-query' : 'urls-list',
		repoQuery: repoQuery ?? '',
		repositories: repositories ? repositories.split(',').filter((r) => r !== '') : [],
		series: series.length > 0 ? series : d.series
	};
}

/** The capture group wizard's values from the URL (`groupSearchQuery` is the query), or null. */
export function captureGroupFieldsFromURL(params: Params): CaptureGroupFields | null {
	const repoQuery = param(params, 'repoQuery');
	const repositories = params.get('repositories');
	const title = param(params, 'title');
	const query = param(params, 'groupSearchQuery');
	if (!repoQuery && !repositories && !title && !query) return null;
	return {
		...defaultCaptureGroupFields(),
		title: title ?? '',
		repoMode: repoQuery ? 'search-query' : 'urls-list',
		repoQuery: repoQuery ?? '',
		repositories: repositories ? repositories.split(',').filter((r) => r !== '') : [],
		query: query ?? ''
	};
}

/** A creation wizard's initial values: URL parameters, else the saved draft, else defaults. */
export function initialFields<T extends InsightType>(
	type: T,
	params: Params,
	storage: DraftStorage | null = browserStorage()
): CreationFields[T] {
	const fromURL =
		type === 'search' ? searchFieldsFromURL(params) : type === 'capture-group' ? captureGroupFieldsFromURL(params) : null;
	if (fromURL) return fromURL as CreationFields[T];
	const draft = loadDraft(type, storage);
	if (draft) return draft;
	const defaults: { [K in InsightType]: () => CreationFields[K] } = {
		search: defaultSearchFields,
		'capture-group': defaultCaptureGroupFields,
		compute: defaultComputeFields,
		'language-stats': defaultLangStatsFields
	};
	return defaults[type]();
}

/**
 * The search results page's "Create trend" link: the search wizard with the query and its
 * pattern type in `query`.
 */
export function createInsightFromSearchURL(query: string, patternType: string): string {
	const q = query.trim();
	const full = /(^|\s)patterntype:/i.test(q) ? q : `${q} patterntype:${patternType}`;
	return `/trends/create/search?${new URLSearchParams({ query: full }).toString()}`;
}
