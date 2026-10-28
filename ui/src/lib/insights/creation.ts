/**
 * The trend creation and edit wizards: form models of the four trend
 * types (search based, detect and track capture groups, group results, language usage), their
 * validation (messages and query checks), the GraphQL inputs they save as,
 * the form values of an existing view, and the live preview's chart models.
 */
import { languagePie, repoFilter, seriesColor, type BarDatum, type LineSeries, type PieDatum, pointLink } from './charts.ts';
import {
	filtersOf,
	insightType,
	toDisplayOptionsInput,
	toFiltersInput,
	withDashboardParam,
	type InsightType
} from './insights.ts';
import type {
	GroupByField,
	InsightPreviewInput,
	InsightViewNode,
	LineChartSearchInsightInput,
	LineChartSeriesInput,
	PieChartSearchInsightInput,
	PreviewSeries,
	RepositoryDefinition,
	RepositoryScopeInput,
	TimeIntervalStepUnit,
	UpdateLineChartSearchInsightInput,
	UpdatePieChartSearchInsightInput,
	ViewControlsInput
} from './queries.ts';

/** The creation routes of each type (`/trends/create/<path>`). */
export const CREATION_PATHS: Record<InsightType, string> = {
	search: 'search',
	'capture-group': 'capture-group',
	compute: 'group-results',
	'language-stats': 'lang-stats'
};

/** The trend type of a creation route's last segment, or null. */
export function creationType(path: string): InsightType | null {
	const e = Object.entries(CREATION_PATHS).find(([, p]) => p === path);
	return e ? (e[0] as InsightType) : null;
}

export function creationURL(type: InsightType | null, dashboardId: string | null): string {
	return withDashboardParam(type ? `/trends/create/${CREATION_PATHS[type]}` : '/trends/create', dashboardId);
}

/** Points a new series records (the default `insights.maximumSampleSize`). */
export const DEFAULT_NUMBER_OF_POINTS = 30;

// ---- Search queries --------------------------------------------------------------------------------

export interface QueryFilter {
	field: string;
	value: string;
	negated: boolean;
}

/** The filters (`field:value`) and boolean operators of a search query. */
export interface ScannedQuery {
	filters: QueryFilter[];
	operators: string[];
	/** The pattern parts (everything but filters and operators). */
	patterns: string[];
}

const FILTER_ALIASES: Record<string, string> = {
	r: 'repo',
	repo: 'repo',
	rev: 'rev',
	revision: 'rev',
	context: 'context',
	type: 'type',
	patterntype: 'patterntype',
	f: 'file',
	file: 'file',
	path: 'file',
	l: 'lang',
	lang: 'lang',
	language: 'lang'
};

/** Splits a query into whitespace separated tokens; quotes and parentheses group. */
export function tokens(query: string): string[] {
	const out: string[] = [];
	let cur = '';
	let quote: string | null = null;
	for (let i = 0; i < query.length; i++) {
		const c = query[i] ?? '';
		if (quote) {
			cur += c;
			if (c === '\\' && i + 1 < query.length) {
				cur += query[++i];
			} else if (c === quote) {
				quote = null;
			}
		} else if (c === '"' || c === "'") {
			quote = c;
			cur += c;
		} else if (/\s/.test(c)) {
			if (cur) out.push(cur);
			cur = '';
		} else {
			cur += c;
		}
	}
	if (cur) out.push(cur);
	return out;
}

export function scanQuery(query: string): ScannedQuery {
	const out: ScannedQuery = { filters: [], operators: [], patterns: [] };
	for (const t of tokens(query)) {
		if (/^(and|or|not)$/i.test(t)) {
			out.operators.push(t.toUpperCase());
			continue;
		}
		const m = /^(-?)([A-Za-z]+):(.*)$/s.exec(t);
		const field = m ? FILTER_ALIASES[(m[2] ?? '').toLowerCase()] : undefined;
		if (m && field) {
			out.filters.push({ field, value: (m[3] ?? '').replace(/^(["'])(.*)\1$/s, '$2'), negated: m[1] === '-' });
		} else {
			out.patterns.push(t);
		}
	}
	return out;
}

/** Whether every query searches commits or diffs (`type:commit`, `type:diff`). */
export function isCommitOrDiffQuery(query: string): boolean {
	return scanQuery(query).filters.some((f) => f.field === 'type' && (f.value === 'commit' || f.value === 'diff'));
}

/**
 * The capture group query checks, as listed under the query field; `undefined`
 * while the query is empty. `hasCaptureGroup`: the server needs one regular
 * expression with a capture group.
 */
export interface QueryChecks {
	hasCaptureGroup: boolean | undefined;
	isValidOperator: boolean | undefined;
	isValidPatternType: boolean | undefined;
	isNotRepo: boolean | undefined;
	isNotContext: boolean | undefined;
	isNotCommitOrDiff: boolean | undefined;
	isNotRev: boolean | undefined;
}

/** Whether a regular expression has a capturing group (`(…)`, `(?P<name>…)`, `(?<name>…)`). */
export function hasCaptureGroup(pattern: string): boolean {
	const unescaped = pattern.replace(/\\./g, '').replace(/\[(?:[^\]\\]|\\.)*\]/g, '');
	return /\((?!\?)|\(\?P?<[A-Za-z_]/.test(unescaped);
}

export function queryChecks(query: string): QueryChecks {
	if (query.trim() === '') {
		return {
			hasCaptureGroup: undefined,
			isValidOperator: undefined,
			isValidPatternType: undefined,
			isNotRepo: undefined,
			isNotContext: undefined,
			isNotCommitOrDiff: undefined,
			isNotRev: undefined
		};
	}
	const q = scanQuery(query);
	const has = (field: string, pred: (v: string) => boolean = (v) => v !== '') =>
		q.filters.some((f) => f.field === field && pred(f.value));
	return {
		hasCaptureGroup: q.patterns.some(hasCaptureGroup),
		isValidOperator: q.operators.length === 0,
		isValidPatternType: !has('patterntype', (v) => ['literal', 'structural', 'standard', 'keyword'].includes(v)),
		isNotRepo: !has('repo'),
		isNotContext: !has('context'),
		isNotCommitOrDiff: !has('type', (v) => v === 'commit' || v === 'diff'),
		isNotRev: !has('rev')
	};
}

// ---- Validation ------------------------------------------------------------------------------------

export function titleError(title: string): string | null {
	return title.trim() === '' ? 'Title is a required field.' : null;
}

export function seriesNameError(name: string): string | null {
	return name.trim() === '' ? 'Name is a required field for data series.' : null;
}

/** A search series' query: required, and without `context:`, `repo:` or `rev:` filters. */
export function seriesQueryError(query: string): string | null {
	if (query.trim() === '') return 'Query is a required field for data series.';
	const c = queryChecks(query);
	if (!c.isNotContext) {
		return 'The `context:` filter is not supported; instead, run over all repositories and use the `context:` on the filter panel after creation';
	}
	if (!c.isNotRepo) {
		return 'Do not include a `repo:` filter; add targeted repositories above, or filter repos on the filter panel after creation';
	}
	if (!c.isNotRev) return 'The `rev:` filter is not currently supported.';
	return null;
}

export function captureGroupQueryError(query: string): string | null {
	if (query.trim() === '') return 'Query is a required field.';
	return Object.values(queryChecks(query)).every(Boolean) ? null : 'Query is not valid';
}

export function repositoriesError(repositories: string[]): string | null {
	return repositories.length === 0 ? 'Repositories is a required field.' : null;
}

export function repoQueryError(query: string): string | null {
	return query.trim() === '' ? 'Search repositories query is a required field, please fill in the field.' : null;
}

export function stepValueError(value: string): string | null {
	const s = value.trim();
	if (s === '') return 'Please specify a step between points.';
	const n = Number(s);
	return Number.isInteger(n) && n >= 1 ? null : 'The step between points must be a whole number of at least 1.';
}

/** The "Other" threshold of a language pie, in percent (1-100). */
export function thresholdError(value: string): string | null {
	const s = value.trim();
	if (s === '') return 'Threshold is a required field.';
	const n = Number(s);
	return Number.isFinite(n) && n >= 1 && n <= 100 ? null : 'Threshold must be between 1 and 100.';
}

export function repositoryError(repository: string): string | null {
	return repository.trim() === '' ? 'Repositories is a required field.' : null;
}

export function repositoryNotFound(repository: string): string {
	return `We couldn't find the repository ${repository.trim()}. Please ensure the repository exists.`;
}

// ---- Form models -------------------------------------------------------------------------------------

/** How the repositories of a trend are chosen: a repository query, a list, or all of them. */
export type RepoMode = 'search-query' | 'urls-list' | 'all-repos';

export interface RepoFields {
	repoMode: RepoMode;
	repoQuery: string;
	repositories: string[];
}

export interface EditableSeries {
	/** A key for the form (stable while editing). */
	key: string;
	/** The series' id on the server, when editing an existing trend. */
	seriesId: string | null;
	name: string;
	query: string;
	stroke: string;
}

let seriesKeys = 0;

export function newSeries(index: number, init: Partial<EditableSeries> = {}): EditableSeries {
	return { key: `series-${++seriesKeys}`, seriesId: null, name: '', query: '', stroke: seriesColor(index), ...init };
}

export function seriesErrors(s: EditableSeries): { name: string | null; query: string | null } {
	return { name: seriesNameError(s.name), query: seriesQueryError(s.query) };
}

export function isSeriesValid(s: EditableSeries): boolean {
	const e = seriesErrors(s);
	return !e.name && !e.query;
}

export function seriesListError(series: EditableSeries[]): string | null {
	if (series.length === 0) return 'No series defined. You must add at least one series to create a trend.';
	if (series.some((s) => !isSeriesValid(s))) return 'Some series is invalid. Remove or edit the invalid series.';
	return null;
}

export type StepUnit = TimeIntervalStepUnit;

export const STEP_UNITS: { unit: StepUnit; label: string }[] = [
	{ unit: 'HOUR', label: 'Hours' },
	{ unit: 'DAY', label: 'Days' },
	{ unit: 'WEEK', label: 'Weeks' },
	{ unit: 'MONTH', label: 'Months' },
	{ unit: 'YEAR', label: 'Years' }
];

export interface SearchInsightFields extends RepoFields {
	title: string;
	series: EditableSeries[];
	stepUnit: StepUnit;
	stepValue: string;
}

export interface CaptureGroupFields extends RepoFields {
	title: string;
	query: string;
	stepUnit: StepUnit;
	stepValue: string;
}

export interface ComputeFields {
	title: string;
	repositories: string[];
	series: EditableSeries[];
	groupBy: GroupByField;
}

export interface LangStatsFields {
	title: string;
	repository: string;
	/** Percent. */
	threshold: string;
}

export function defaultSearchFields(): SearchInsightFields {
	return {
		title: '',
		repoMode: 'search-query',
		repoQuery: '',
		repositories: [],
		series: [newSeries(0)],
		stepUnit: 'MONTH',
		stepValue: '1'
	};
}

export function defaultCaptureGroupFields(): CaptureGroupFields {
	return {
		title: '',
		repoMode: 'search-query',
		repoQuery: '',
		repositories: [],
		query: '',
		stepUnit: 'MONTH',
		stepValue: '2'
	};
}

export function defaultComputeFields(): ComputeFields {
	return { title: '', repositories: [], series: [newSeries(0)], groupBy: 'REPO' };
}

export function defaultLangStatsFields(): LangStatsFields {
	return { title: '', repository: '', threshold: '3' };
}

/** The repository section's errors (the async repository query check aside). */
export function repoFieldsError(f: RepoFields): string | null {
	switch (f.repoMode) {
		case 'search-query':
			return repoQueryError(f.repoQuery);
		case 'urls-list':
			return repositoriesError(f.repositories);
		case 'all-repos':
			return null;
	}
}

/** Every error of a form, by field; empty when it can be saved. */
export function searchFormErrors(f: SearchInsightFields): Record<string, string> {
	return compact({
		repositories: repoFieldsError(f),
		series: seriesListError(f.series),
		title: titleError(f.title),
		step: stepValueError(f.stepValue)
	});
}

export function captureGroupFormErrors(f: CaptureGroupFields): Record<string, string> {
	return compact({
		repositories: repoFieldsError(f),
		query: captureGroupQueryError(f.query),
		title: titleError(f.title),
		step: stepValueError(f.stepValue)
	});
}

export function computeFormErrors(f: ComputeFields): Record<string, string> {
	return compact({
		repositories: repositoriesError(f.repositories),
		series: seriesListError(f.series),
		title: titleError(f.title)
	});
}

export function langStatsFormErrors(f: LangStatsFields): Record<string, string> {
	return compact({
		repository: repositoryError(f.repository),
		title: titleError(f.title),
		threshold: thresholdError(f.threshold)
	});
}

function compact(errors: Record<string, string | null>): Record<string, string> {
	return Object.fromEntries(Object.entries(errors).filter((e): e is [string, string] => e[1] !== null));
}

// ---- Group results ------------------------------------------------------------------------------

export const GROUP_BY_OPTIONS: { value: GroupByField; label: string }[] = [
	{ value: 'REPO', label: 'repository' },
	{ value: 'LANG', label: 'language' },
	{ value: 'PATH', label: 'path' },
	{ value: 'AUTHOR', label: 'author' },
	{ value: 'DATE', label: 'date' }
];

/**
 * Why a group is not available for these series, or null: author and date need commit or
 * diff searches, language and path need content searches (the server's rule).
 */
export function groupByUnavailable(groupBy: GroupByField, series: { query: string }[]): string | null {
	const queries = series.map((s) => s.query).filter((q) => q.trim() !== '');
	const commits = queries.length > 0 && queries.every(isCommitOrDiffQuery);
	if ((groupBy === 'AUTHOR' || groupBy === 'DATE') && !commits) {
		return 'Available only for queries with type:commit and type:diff';
	}
	if ((groupBy === 'LANG' || groupBy === 'PATH') && queries.some(isCommitOrDiffQuery)) {
		return 'Not available for queries with type:commit and type:diff';
	}
	return null;
}

// ---- Step description ----------------------------------------------------------------------------

const UNIT_MINUTES: Record<StepUnit, number> = {
	YEAR: 60 * 24 * 7 * 5 * 12,
	MONTH: 60 * 24 * 7 * 5,
	WEEK: 60 * 24 * 7,
	DAY: 60 * 24,
	HOUR: 60
};
const UNIT_ORDER: StepUnit[] = ['YEAR', 'MONTH', 'WEEK', 'DAY', 'HOUR'];

function unitText(n: number, unit: StepUnit): string {
	const word = unit.toLowerCase();
	return `${n} ${word}${n === 1 ? '' : 's'}`;
}

/** "3 years and 6 months": a duration in the largest units (a month is 5 weeks). */
export function formatDuration(value: number, unit: StepUnit): string {
	let minutes = value * UNIT_MINUTES[unit];
	const parts: string[] = [];
	for (const u of UNIT_ORDER) {
		const n = Math.floor(minutes / UNIT_MINUTES[u]);
		minutes -= n * UNIT_MINUTES[u];
		if (n !== 0) parts.push(unitText(n, u));
	}
	if (parts.length <= 1) return parts[0] ?? '';
	return `${parts.slice(0, -1).join(', ')} and ${parts[parts.length - 1]}`;
}

/** The granularity hint: "Show the past … of data, one datapoint every …". */
export function stepDescription(value: string, unit: StepUnit, points = DEFAULT_NUMBER_OF_POINTS): string {
	const n = Number(value.trim());
	if (!Number.isInteger(n) || n < 1) return '';
	const every = n === 1 ? unit.toLowerCase() : unitText(n, unit);
	return `Show the past ${formatDuration(n * (points - 1), unit)} of data, one datapoint every ${every}. This trend provides ${points} datapoints.`;
}

// ---- GraphQL inputs ------------------------------------------------------------------------------

export function repositoryScopeInput(f: RepoFields): RepositoryScopeInput {
	switch (f.repoMode) {
		case 'search-query':
			return { repositories: [], repositoryCriteria: f.repoQuery.trim() || null };
		case 'urls-list':
			return { repositories: f.repositories, repositoryCriteria: null };
		case 'all-repos':
			return { repositories: [], repositoryCriteria: null };
	}
}

function stepInput(f: { stepUnit: StepUnit; stepValue: string }): { stepInterval: { unit: StepUnit; value: number } } {
	return { stepInterval: { unit: f.stepUnit, value: Math.max(1, Math.trunc(Number(f.stepValue)) || 1) } };
}

/** View controls of a new trend: no filters, sorted by result count. */
export const DEFAULT_VIEW_CONTROLS: ViewControlsInput = {
	filters: { includeRepoRegex: null, excludeRepoRegex: null, searchContexts: [] },
	seriesDisplayOptions: { sortOptions: { mode: 'RESULT_COUNT', direction: 'DESC' }, limit: null, numSamples: null }
};

/** The view controls an edit keeps: the view's saved filters and display options. */
export function viewControlsOf(view: InsightViewNode): ViewControlsInput {
	const f = filtersOf(view.defaultFilters, view.defaultSeriesDisplayOptions);
	return { filters: toFiltersInput(f), seriesDisplayOptions: toDisplayOptionsInput(f) };
}

function searchSeries(f: SearchInsightFields): LineChartSeriesInput[] {
	return f.series.map((s) => ({
		seriesId: s.seriesId,
		query: s.query.trim(),
		options: { label: s.name.trim(), lineColor: s.stroke },
		timeScope: stepInput(f),
		generatedFromCaptureGroups: false,
		groupBy: null
	}));
}

function captureGroupSeries(f: CaptureGroupFields, seriesId: string | null): LineChartSeriesInput[] {
	return [
		{
			seriesId,
			query: f.query.trim(),
			options: { label: null, lineColor: null },
			timeScope: stepInput(f),
			generatedFromCaptureGroups: true,
			groupBy: null
		}
	];
}

/** Group-by series record every two weeks (the bar chart sums them). */
const COMPUTE_TIME_SCOPE = { stepInterval: { unit: 'WEEK' as const, value: 2 } };

function computeSeries(f: ComputeFields): LineChartSeriesInput[] {
	return f.series.map((s) => ({
		seriesId: s.seriesId,
		query: s.query.trim(),
		options: { label: s.name.trim(), lineColor: s.stroke },
		repositoryScope: { repositories: f.repositories, repositoryCriteria: null },
		timeScope: COMPUTE_TIME_SCOPE,
		generatedFromCaptureGroups: true,
		groupBy: f.groupBy
	}));
}

function dashboards(dashboardId: string | null): { dashboards?: string[] } {
	return dashboardId ? { dashboards: [dashboardId] } : {};
}

export function searchCreateInput(f: SearchInsightFields, dashboardId: string | null): LineChartSearchInsightInput {
	return {
		dataSeries: searchSeries(f),
		repositoryScope: repositoryScopeInput(f),
		options: { title: f.title.trim() },
		viewControls: DEFAULT_VIEW_CONTROLS,
		...dashboards(dashboardId)
	};
}

export function searchUpdateInput(f: SearchInsightFields, controls: ViewControlsInput): UpdateLineChartSearchInsightInput {
	return {
		dataSeries: searchSeries(f),
		repositoryScope: repositoryScopeInput(f),
		presentationOptions: { title: f.title.trim() },
		viewControls: controls
	};
}

export function captureGroupCreateInput(f: CaptureGroupFields, dashboardId: string | null): LineChartSearchInsightInput {
	return {
		dataSeries: captureGroupSeries(f, null),
		repositoryScope: repositoryScopeInput(f),
		options: { title: f.title.trim() },
		viewControls: DEFAULT_VIEW_CONTROLS,
		...dashboards(dashboardId)
	};
}

export function captureGroupUpdateInput(
	f: CaptureGroupFields,
	seriesId: string | null,
	controls: ViewControlsInput
): UpdateLineChartSearchInsightInput {
	return {
		dataSeries: captureGroupSeries(f, seriesId),
		repositoryScope: repositoryScopeInput(f),
		presentationOptions: { title: f.title.trim() },
		viewControls: controls
	};
}

export function computeCreateInput(f: ComputeFields, dashboardId: string | null): LineChartSearchInsightInput {
	return {
		dataSeries: computeSeries(f),
		repositoryScope: { repositories: f.repositories, repositoryCriteria: null },
		options: { title: f.title.trim() },
		viewControls: DEFAULT_VIEW_CONTROLS,
		...dashboards(dashboardId)
	};
}

export function computeUpdateInput(f: ComputeFields, controls: ViewControlsInput): UpdateLineChartSearchInsightInput {
	return {
		dataSeries: computeSeries(f),
		repositoryScope: { repositories: f.repositories, repositoryCriteria: null },
		presentationOptions: { title: f.title.trim() },
		viewControls: controls
	};
}

function thresholdFraction(f: LangStatsFields): number {
	return Math.min(Math.max(Number(f.threshold) / 100, 0), 1);
}

export function langStatsCreateInput(f: LangStatsFields, dashboardId: string | null): PieChartSearchInsightInput {
	return { ...langStatsUpdateInput(f), ...dashboards(dashboardId) };
}

export function langStatsUpdateInput(f: LangStatsFields): UpdatePieChartSearchInsightInput {
	return {
		// The pie has no query setting; the API keeps the field.
		query: '',
		repositoryScope: { repositories: [f.repository.trim()], repositoryCriteria: null },
		presentationOptions: { title: f.title.trim(), otherThreshold: thresholdFraction(f) }
	};
}

// ---- Editing an existing view ---------------------------------------------------------------------

function repoFieldsOf(def: RepositoryDefinition | undefined): RepoFields {
	if (!def) return { repoMode: 'all-repos', repoQuery: '', repositories: [] };
	if (def.__typename === 'InsightRepositoryScope') {
		return { repoMode: 'urls-list', repoQuery: '', repositories: [...def.repositories] };
	}
	if (def.allRepositories || def.search.trim() === '') return { repoMode: 'all-repos', repoQuery: '', repositories: [] };
	return { repoMode: 'search-query', repoQuery: def.search, repositories: [] };
}

/** An existing view's form: its type and the values the matching wizard starts from. */
export type EditForm =
	| { type: 'search'; fields: SearchInsightFields }
	| { type: 'capture-group'; fields: CaptureGroupFields; seriesId: string | null }
	| { type: 'compute'; fields: ComputeFields }
	| { type: 'language-stats'; fields: LangStatsFields };

export function editFormOf(view: InsightViewNode): EditForm {
	const type = insightType(view);
	const defs = view.dataSeriesDefinitions;
	const first = defs[0];
	const presentation = view.presentation;
	const seriesPresentation =
		presentation.__typename === 'LineChartInsightViewPresentation' ? presentation.seriesPresentation : [];
	const series = defs.map((d, i) => {
		const p = seriesPresentation.find((s) => s.seriesId === d.seriesId);
		return newSeries(i, {
			seriesId: d.seriesId,
			name: p?.label ?? '',
			query: d.query,
			stroke: p?.color || seriesColor(i)
		});
	});
	const step = {
		stepUnit: first?.timeScope.unit ?? 'MONTH',
		stepValue: String(first?.timeScope.value ?? 1)
	};
	switch (type) {
		case 'language-stats': {
			const def = first?.repositoryDefinition;
			const otherThreshold = presentation.__typename === 'PieChartInsightViewPresentation' ? presentation.otherThreshold : 0.03;
			return {
				type,
				fields: {
					title: presentation.title,
					repository: def?.__typename === 'InsightRepositoryScope' ? (def.repositories[0] ?? '') : '',
					threshold: String(Math.round(otherThreshold * 1000) / 10)
				}
			};
		}
		case 'compute': {
			const def = first?.repositoryDefinition;
			return {
				type,
				fields: {
					title: presentation.title,
					repositories: def?.__typename === 'InsightRepositoryScope' ? [...def.repositories] : [],
					series,
					groupBy: first?.groupBy ?? 'REPO'
				}
			};
		}
		case 'capture-group':
			return {
				type,
				seriesId: first?.seriesId ?? null,
				fields: { title: presentation.title, ...repoFieldsOf(first?.repositoryDefinition), query: first?.query ?? '', ...step }
			};
		case 'search':
			return {
				type,
				fields: { title: presentation.title, ...repoFieldsOf(first?.repositoryDefinition), series, ...step }
			};
	}
}

// ---- Live previews ----------------------------------------------------------------------------------

/** What a live preview draws. */
export type PreviewModel =
	| { kind: 'line'; series: LineSeries[] }
	| { kind: 'bar'; data: BarDatum[] }
	| { kind: 'pie'; data: PieDatum[] };

/** Whether a preview has nothing to draw ("No data to display"). */
export function previewModelEmpty(m: PreviewModel): boolean {
	switch (m.kind) {
		case 'line':
			return m.series.every((s) => s.points.length === 0);
		case 'bar':
			return m.data.every((d) => d.value === 0);
		case 'pie':
			return m.data.length === 0;
	}
}

/** The preview of search series: the valid ones, over the form's repositories. */
export function searchPreviewInput(f: SearchInsightFields): InsightPreviewInput {
	return {
		repositoryScope: repositoryScopeInput(f),
		timeScope: stepInput(f),
		series: f.series
			.filter(isSeriesValid)
			.map((s) => ({ query: s.query.trim(), label: s.name.trim(), generatedFromCaptureGroups: false, groupBy: null }))
	};
}

export function captureGroupPreviewInput(f: CaptureGroupFields): InsightPreviewInput {
	return {
		repositoryScope: repositoryScopeInput(f),
		timeScope: stepInput(f),
		series: [{ query: f.query.trim(), label: '', generatedFromCaptureGroups: true, groupBy: null }]
	};
}

export function computePreviewInput(f: ComputeFields): InsightPreviewInput {
	return {
		repositoryScope: { repositories: f.repositories, repositoryCriteria: null },
		// Grouped series are counted at the latest point only; the interval is required.
		timeScope: { stepInterval: { unit: 'DAY', value: 1 } },
		series: f.series
			.filter(isSeriesValid)
			.map((s) => ({ query: s.query.trim(), label: s.name.trim(), generatedFromCaptureGroups: true, groupBy: f.groupBy }))
	};
}

/**
 * Line series of a preview. Search series keep their form colors (matched by label and
 * position), generated ones take the default colors; at most `max` series.
 */
export function previewLineSeries(
	result: PreviewSeries[],
	strokes: { label: string; stroke: string }[],
	max = 20
): LineSeries[] {
	return result.slice(0, max).map((s, i) => {
		const input = strokes[i];
		return {
			id: String(i),
			name: s.label,
			color: input && input.label === s.label ? input.stroke : seriesColor(i),
			points: s.points
				.map((p) => ({ x: Date.parse(p.dateTime), y: p.value, link: pointLink(p) }))
				.filter((p) => Number.isFinite(p.x))
				.sort((a, b) => a.x - b.x),
			alerts: []
		};
	});
}

/** Bars of a group-by preview: values summed by group, colored as the first series. */
export function previewBarData(result: PreviewSeries[], color: string): BarDatum[] {
	const groups = new Map<string, number>();
	for (const s of result) {
		if (!s.label) continue;
		groups.set(s.label, (groups.get(s.label) ?? 0) + s.points.reduce((n, p) => n + p.value, 0));
	}
	return [...groups].map(([name, value]) => ({ name, value, color }));
}

/** Slices of a language usage preview. */
export function previewPieData(
	stats: { name: string; totalLines: number }[],
	repository: string,
	thresholdPercent: string
): PieDatum[] {
	const threshold = Math.min(Math.max(Number(thresholdPercent) / 100 || 0, 0), 1);
	return languagePie(stats, threshold, repoFilter([repository.trim()]));
}
