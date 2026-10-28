/**
 * Pure helpers of the trends pages: dashboard visibility
 * and grouping, the per-view filter model, and the inputs that save filters.
 */
import type {
	DashboardNode,
	FiltersInput,
	InsightViewNode,
	OrgRef,
	PermissionGrants,
	PermissionGrantsInput,
	RepositoryDefinition,
	SeriesDisplayOptions,
	SeriesDisplayOptionsInput,
	SeriesSortDirection,
	SeriesSortMode,
	UpdateLineChartSearchInsightInput
} from './queries.ts';

/** The pseudo dashboard id of the "All trends" tab. */
export const ALL_INSIGHTS = 'all';

/** The most series a view shows. */
export const MAX_NUMBER_OF_SERIES = 20;
/** The most points per series (the server's limit). */
export const MAX_NUMBER_OF_SAMPLES = 90;

/** localStorage key of the last dashboard opened. */
export const LAST_DASHBOARD_KEY = 'insights.lastVisitedDashboardId';

export function dashboardURL(id: string): string {
	return `/trends/dashboards/${encodeURIComponent(id)}`;
}

export function insightURL(id: string): string {
	return `/trends/${encodeURIComponent(id)}`;
}

/** A URL with `?dashboardId=` (creation pages add the new trend to that dashboard). */
export function withDashboardParam(url: string, dashboardId: string | null | undefined): string {
	if (!dashboardId || dashboardId === ALL_INSIGHTS) return url;
	return `${url}?dashboardId=${encodeURIComponent(dashboardId)}`;
}

// ---- Dashboard visibility ----------------------------------------------------------------------

export type DashboardKind = 'personal' | 'organization' | 'global';

/** Global wins over organizations, which win over users. */
export function dashboardKind(grants: PermissionGrants): DashboardKind {
	if (grants.global) return 'global';
	if (grants.organizations.length > 0) return 'organization';
	return 'personal';
}

export function orgTitle(org: OrgRef): string {
	return org.displayName || org.name;
}

/** The badge next to a dashboard's name: "Private", "Global" or its organizations. */
export function dashboardOwnerName(dashboard: DashboardNode, orgs: OrgRef[]): string {
	switch (dashboardKind(dashboard.grants)) {
		case 'personal':
			return 'Private';
		case 'global':
			return 'Global';
		case 'organization':
			return dashboard.grants.organizations
				.map((id) => {
					const org = orgs.find((o) => o.id === id);
					return org ? orgTitle(org) : 'Organization';
				})
				.join(' ');
	}
}

export interface DashboardGroup {
	/** Stable key: `private`, `global` or the organization id. */
	key: string;
	heading: string;
	dashboards: DashboardNode[];
}

/**
 * Dashboards whose title contains `find` (case-insensitive), grouped as the
 * dashboard picker shows them: Private, Global, then one group per organization (a dashboard granted to
 * several organizations is in each).
 */
export function groupDashboards(dashboards: DashboardNode[], orgs: OrgRef[], find = ''): DashboardGroup[] {
	const needle = find.trim().toLowerCase();
	const matching = needle ? dashboards.filter((d) => d.title.toLowerCase().includes(needle)) : dashboards;
	const groups: DashboardGroup[] = [];
	const personal = matching.filter((d) => dashboardKind(d.grants) === 'personal');
	if (personal.length) groups.push({ key: 'private', heading: 'Private', dashboards: personal });
	const global = matching.filter((d) => dashboardKind(d.grants) === 'global');
	if (global.length) groups.push({ key: 'global', heading: 'Global', dashboards: global });
	const byOrg = new Map<string, DashboardGroup>();
	for (const d of matching) {
		if (dashboardKind(d.grants) !== 'organization') continue;
		for (const id of d.grants.organizations) {
			let group = byOrg.get(id);
			if (!group) {
				const org = orgs.find((o) => o.id === id);
				group = { key: id, heading: org ? orgTitle(org) : 'Organization', dashboards: [] };
				byOrg.set(id, group);
			}
			group.dashboards.push(d);
		}
	}
	groups.push(...byOrg.values());
	return groups;
}

/** Who a dashboard is shared with: the form's "Visibility" choice. */
export type Visibility = { kind: 'personal' } | { kind: 'organization'; id: string } | { kind: 'global' };

export function visibilityKey(v: Visibility): string {
	return v.kind === 'organization' ? `org:${v.id}` : v.kind;
}

export function grantsInput(v: Visibility, userId: string): PermissionGrantsInput {
	switch (v.kind) {
		case 'personal':
			return { users: [userId], organizations: [], global: false };
		case 'organization':
			return { users: [], organizations: [v.id], global: false };
		case 'global':
			return { users: [], organizations: [], global: true };
	}
}

/**
 * The visibility to preselect when editing a dashboard: global if it is, else the first of its
 * organizations the viewer is in, else private.
 */
export function visibilityOf(grants: PermissionGrants, orgs: OrgRef[]): Visibility {
	if (grants.global) return { kind: 'global' };
	const org = grants.organizations.find((id) => orgs.some((o) => o.id === id));
	if (org) return { kind: 'organization', id: org };
	return { kind: 'personal' };
}

// ---- View filters --------------------------------------------------------------------------------

/** The drill-down filters of a card: the form values of the filters panel. */
export interface InsightFilters {
	context: string;
	includeRepoRegexp: string;
	excludeRepoRegexp: string;
	seriesDisplayOptions: {
		limit: number | null;
		numSamples: number | null;
		sortOptions: { mode: SeriesSortMode; direction: SeriesSortDirection };
	};
}

export const DEFAULT_SORT = { mode: 'RESULT_COUNT', direction: 'DESC' } as const;

export const EMPTY_FILTERS: InsightFilters = {
	context: '',
	includeRepoRegexp: '',
	excludeRepoRegexp: '',
	seriesDisplayOptions: { limit: null, numSamples: null, sortOptions: { ...DEFAULT_SORT } }
};

/** The filters saved on a view. */
export function filtersOf(
	filters: InsightViewNode['defaultFilters'],
	display: SeriesDisplayOptions
): InsightFilters {
	return {
		context: filters.searchContexts?.[0] ?? '',
		includeRepoRegexp: filters.includeRepoRegex ?? '',
		excludeRepoRegexp: filters.excludeRepoRegex ?? '',
		seriesDisplayOptions: {
			limit: display.limit ?? null,
			numSamples: display.numSamples ?? null,
			sortOptions: {
				mode: display.sortOptions.mode ?? DEFAULT_SORT.mode,
				direction: display.sortOptions.direction ?? DEFAULT_SORT.direction
			}
		}
	};
}

export function filtersEqual(a: InsightFilters, b: InsightFilters): boolean {
	return (
		a.context.trim() === b.context.trim() &&
		a.includeRepoRegexp.trim() === b.includeRepoRegexp.trim() &&
		a.excludeRepoRegexp.trim() === b.excludeRepoRegexp.trim() &&
		displayOptionsEqual(a.seriesDisplayOptions, b.seriesDisplayOptions)
	);
}

export function displayOptionsEqual(
	a: InsightFilters['seriesDisplayOptions'],
	b: InsightFilters['seriesDisplayOptions']
): boolean {
	return (
		a.limit === b.limit &&
		a.numSamples === b.numSamples &&
		a.sortOptions.mode === b.sortOptions.mode &&
		a.sortOptions.direction === b.sortOptions.direction
	);
}

/** Whether any filter differs from "no filters" (the filter button's active state). */
export function hasActiveFilters(f: InsightFilters): boolean {
	return (
		[f.context, f.includeRepoRegexp, f.excludeRepoRegexp].some((s) => s.trim() !== '') ||
		!displayOptionsEqual(f.seriesDisplayOptions, EMPTY_FILTERS.seriesDisplayOptions)
	);
}

export function toFiltersInput(f: InsightFilters): FiltersInput {
	const context = f.context.trim();
	return {
		includeRepoRegex: f.includeRepoRegexp.trim() || null,
		excludeRepoRegex: f.excludeRepoRegexp.trim() || null,
		searchContexts: context ? [context] : []
	};
}

export function toDisplayOptionsInput(f: InsightFilters): SeriesDisplayOptionsInput {
	const d = f.seriesDisplayOptions;
	return { sortOptions: { ...d.sortOptions }, limit: d.limit, numSamples: d.numSamples };
}

/** A number input's value clamped to 1…max; empty means "not set". */
export function clampCount(input: string, max: number): number | null {
	const s = input.trim();
	if (s === '') return null;
	const n = Number.parseInt(s, 10);
	if (Number.isNaN(n)) return null;
	return Math.max(Math.min(n, max), 1);
}

/** An error message for an invalid repository regular expression, or null. */
export function repoRegexError(value: string): string | null {
	const s = value.trim();
	if (!s) return null;
	try {
		new RegExp(s);
		return null;
	} catch {
		return 'Must be a valid regular expression.';
	}
}

const SORT_LABELS: Record<SeriesSortMode, Record<SeriesSortDirection, string>> = {
	RESULT_COUNT: { DESC: 'Highest result count', ASC: 'Lowest result count' },
	LEXICOGRAPHICAL: { ASC: 'Name A-Z', DESC: 'Name Z-A' },
	DATE_ADDED: { DESC: 'Newest', ASC: 'Oldest' }
};

/** The "Sort & Limit" section preview, e.g. "Highest result count, limit 10". */
export function sortAndLimitSummary(d: InsightFilters['seriesDisplayOptions']): string {
	const parts = [SORT_LABELS[d.sortOptions.mode][d.sortOptions.direction]];
	if (d.limit !== null) parts.push(`limit ${d.limit}`);
	if (d.numSamples !== null) parts.push(`${d.numSamples} points`);
	return parts.join(', ');
}

/** The "Regular expression" section preview, e.g. "repo:a -repo:b". */
export function repositoriesSummary(f: InsightFilters): string {
	const parts: string[] = [];
	if (f.includeRepoRegexp.trim()) parts.push(`repo:${f.includeRepoRegexp.trim()}`);
	if (f.excludeRepoRegexp.trim()) parts.push(`-repo:${f.excludeRepoRegexp.trim()}`);
	return parts.join(' ');
}

// ---- Views ---------------------------------------------------------------------------------------

export type InsightType = 'search' | 'capture-group' | 'compute' | 'language-stats';

/** The trend types: search based, detect and track (capture groups), group by, pie. */
export function insightType(view: {
	presentation: { __typename: string };
	dataSeriesDefinitions: { generatedFromCaptureGroups: boolean; groupBy: string | null }[];
}): InsightType {
	if (view.presentation.__typename === 'PieChartInsightViewPresentation') return 'language-stats';
	const first = view.dataSeriesDefinitions[0];
	if (first?.groupBy) return 'compute';
	if (first?.generatedFromCaptureGroups) return 'capture-group';
	return 'search';
}

export const INSIGHT_TYPE_LABELS: Record<InsightType, string> = {
	search: 'Search based',
	'capture-group': 'Detect and track',
	compute: 'Group results',
	'language-stats': 'Language usage'
};

/** Whether a card has the drill-down filters (line charts; pie charts have none). */
export function hasFilters(view: InsightViewNode): boolean {
	return view.presentation.__typename === 'LineChartInsightViewPresentation';
}

/** Whether the points-per-series option applies (not to group-by trends). */
export function hasNumSamples(view: InsightViewNode): boolean {
	return insightType(view) !== 'compute';
}

function repositoryScope(def: RepositoryDefinition): { repositories: string[]; repositoryCriteria: string | null } {
	if (def.__typename === 'InsightRepositoryScope') {
		return { repositories: def.repositories, repositoryCriteria: null };
	}
	return { repositories: [], repositoryCriteria: def.allRepositories ? null : def.search || null };
}

/**
 * The input that saves `filters` as a line chart's defaults and keeps everything else: every
 * series by ID with its query, scope, interval and presentation.
 */
export function updateFiltersInput(view: InsightViewNode, filters: InsightFilters): UpdateLineChartSearchInsightInput {
	const presentation = view.presentation;
	const seriesPresentation =
		presentation.__typename === 'LineChartInsightViewPresentation' ? presentation.seriesPresentation : [];
	return {
		dataSeries: view.dataSeriesDefinitions.map((d) => {
			const p = seriesPresentation.find((s) => s.seriesId === d.seriesId);
			return {
				seriesId: d.seriesId,
				query: d.query,
				options: { label: p?.label ?? '', lineColor: p?.color ?? '' },
				repositoryScope: repositoryScope(d.repositoryDefinition),
				timeScope: { stepInterval: { unit: d.timeScope.unit, value: d.timeScope.value } },
				generatedFromCaptureGroups: d.generatedFromCaptureGroups,
				groupBy: d.groupBy
			};
		}),
		presentationOptions: { title: presentation.title },
		viewControls: { filters: toFiltersInput(filters), seriesDisplayOptions: toDisplayOptionsInput(filters) }
	};
}

/** Whether any series is still backfilling ("Datapoints shown may be undercounted"). */
export function isLoadingData(view: InsightViewNode): boolean {
	return view.dataSeries.some((s) => s.status.isLoadingData === true || s.status.pendingJobs > 0);
}

/** Language shares of a pie chart: languages under `otherThreshold` are summed as "Other". */
export function languageShares(
	stats: { name: string; totalLines: number }[],
	otherThreshold: number
): { name: string; lines: number; share: number }[] {
	const total = stats.reduce((n, s) => n + s.totalLines, 0);
	if (total === 0) return [];
	const out: { name: string; lines: number; share: number }[] = [];
	let other = 0;
	for (const s of stats) {
		const share = s.totalLines / total;
		if (share < otherThreshold) other += s.totalLines;
		else out.push({ name: s.name, lines: s.totalLines, share });
	}
	if (other > 0) out.push({ name: 'Other', lines: other, share: other / total });
	return out;
}

/** Ids to add and to remove so that a dashboard holds `next` instead of `prev`. */
export function viewChanges(prev: string[], next: string[]): { add: string[]; remove: string[] } {
	return {
		add: next.filter((id) => !prev.includes(id)),
		remove: prev.filter((id) => !next.includes(id))
	};
}

/** Reads the last opened dashboard id; storage may be unavailable. */
export function lastVisitedDashboard(): string | null {
	try {
		return localStorage.getItem(LAST_DASHBOARD_KEY);
	} catch {
		return null;
	}
}

export function rememberDashboard(id: string | null): void {
	try {
		if (id) localStorage.setItem(LAST_DASHBOARD_KEY, id);
		else localStorage.removeItem(LAST_DASHBOARD_KEY);
	} catch {
		// Private mode or blocked storage: nothing to remember.
	}
}
