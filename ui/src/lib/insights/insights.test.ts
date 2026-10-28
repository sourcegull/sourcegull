import { describe, expect, it } from 'vitest';
import {
	EMPTY_FILTERS,
	clampCount,
	dashboardKind,
	dashboardOwnerName,
	filtersEqual,
	filtersOf,
	grantsInput,
	groupDashboards,
	hasActiveFilters,
	insightType,
	languageShares,
	repoRegexError,
	repositoriesSummary,
	sortAndLimitSummary,
	toDisplayOptionsInput,
	toFiltersInput,
	updateFiltersInput,
	viewChanges,
	visibilityOf,
	withDashboardParam,
	type InsightFilters
} from './insights.ts';
import type { DashboardNode, InsightViewNode, OrgRef } from './queries.ts';

const orgs: OrgRef[] = [
	{ id: 'O1', name: 'acme', displayName: 'Acme Corp' },
	{ id: 'O2', name: 'beta', displayName: null }
];

function dashboard(id: string, title: string, grants: Partial<DashboardNode['grants']>): DashboardNode {
	return { id, title, grants: { users: [], organizations: [], global: false, ...grants } };
}

function view(overrides: Partial<InsightViewNode> = {}): InsightViewNode {
	const display = { sortOptions: { mode: null, direction: null }, limit: null, numSamples: null };
	const filters = { includeRepoRegex: null, excludeRepoRegex: null, searchContexts: null };
	return {
		id: 'V1',
		isFrozen: false,
		dashboardReferenceCount: 1,
		seriesCount: 2,
		presentation: {
			__typename: 'LineChartInsightViewPresentation',
			title: 'TODOs',
			seriesPresentation: [
				{ seriesId: 's1', label: 'TODO', color: '#f00' },
				{ seriesId: 's2', label: 'FIXME', color: '#0f0' }
			]
		},
		defaultFilters: filters,
		appliedFilters: filters,
		defaultSeriesDisplayOptions: display,
		appliedSeriesDisplayOptions: display,
		dataSeriesDefinitions: [
			{
				__typename: 'SearchInsightDataSeriesDefinition',
				seriesId: 's1',
				query: 'TODO',
				repositoryDefinition: { __typename: 'InsightRepositoryScope', repositories: ['a/b'] },
				timeScope: { __typename: 'InsightIntervalTimeScope', unit: 'WEEK', value: 2 },
				generatedFromCaptureGroups: false,
				isCalculated: true,
				groupBy: null
			},
			{
				__typename: 'SearchInsightDataSeriesDefinition',
				seriesId: 's2',
				query: 'FIXME',
				repositoryDefinition: { __typename: 'RepositorySearchScope', search: 'repo:^x', allRepositories: false },
				timeScope: { __typename: 'InsightIntervalTimeScope', unit: 'MONTH', value: 1 },
				generatedFromCaptureGroups: false,
				isCalculated: true,
				groupBy: null
			}
		],
		dataSeries: [
			{
				seriesId: 's1',
				label: 'TODO',
				points: [
					{ dateTime: '2026-01-01T00:00:00Z', value: 3, diffQuery: null, pointInTimeQuery: null },
					{ dateTime: '2026-02-01T00:00:00Z', value: 5, diffQuery: null, pointInTimeQuery: null }
				],
				status: {
					isLoadingData: false,
					pendingJobs: 0,
					completedJobs: 1,
					failedJobs: 0,
					backfillQueuedAt: null,
					incompleteDatapoints: []
				}
			}
		],
		languageStatistics: [],
		...overrides
	};
}

describe('dashboards', () => {
	it('classifies dashboards by their grants', () => {
		expect(dashboardKind({ users: ['U1'], organizations: [], global: false })).toBe('personal');
		expect(dashboardKind({ users: [], organizations: ['O1'], global: false })).toBe('organization');
		expect(dashboardKind({ users: ['U1'], organizations: ['O1'], global: true })).toBe('global');
	});

	it('names dashboard owners for the badge', () => {
		expect(dashboardOwnerName(dashboard('D1', 'a', { users: ['U1'] }), orgs)).toBe('Private');
		expect(dashboardOwnerName(dashboard('D1', 'a', { global: true }), orgs)).toBe('Global');
		expect(dashboardOwnerName(dashboard('D1', 'a', { organizations: ['O1', 'O2'] }), orgs)).toBe('Acme Corp beta');
		expect(dashboardOwnerName(dashboard('D1', 'a', { organizations: ['O9'] }), orgs)).toBe('Organization');
	});

	it('groups dashboards: private, global, then each organization, filtered by title', () => {
		const list = [
			dashboard('D1', 'Mine', { users: ['U1'] }),
			dashboard('D2', 'Everyone', { global: true }),
			dashboard('D3', 'Shared', { organizations: ['O1', 'O2'] }),
			dashboard('D4', 'Team', { organizations: ['O2'] })
		];
		const groups = groupDashboards(list, orgs);
		expect(groups.map((g) => [g.heading, g.dashboards.map((d) => d.id)])).toEqual([
			['Private', ['D1']],
			['Global', ['D2']],
			['Acme Corp', ['D3']],
			['beta', ['D3', 'D4']]
		]);
		expect(groupDashboards(list, orgs, ' TEAM ').map((g) => g.heading)).toEqual(['beta']);
		expect(groupDashboards(list, orgs, 'nothing')).toEqual([]);
	});

	it('turns a visibility into grants and back', () => {
		expect(grantsInput({ kind: 'personal' }, 'U1')).toEqual({ users: ['U1'], organizations: [], global: false });
		expect(grantsInput({ kind: 'organization', id: 'O2' }, 'U1')).toEqual({
			users: [],
			organizations: ['O2'],
			global: false
		});
		expect(grantsInput({ kind: 'global' }, 'U1')).toEqual({ users: [], organizations: [], global: true });
		expect(visibilityOf({ users: [], organizations: ['O9', 'O2'], global: false }, orgs)).toEqual({
			kind: 'organization',
			id: 'O2'
		});
		expect(visibilityOf({ users: [], organizations: ['O9'], global: false }, orgs)).toEqual({ kind: 'personal' });
		expect(visibilityOf({ users: [], organizations: ['O1'], global: true }, orgs)).toEqual({ kind: 'global' });
	});

	it('adds the dashboard to creation links, except for All trends', () => {
		expect(withDashboardParam('/trends/create', 'RDE=')).toBe('/trends/create?dashboardId=RDE%3D');
		expect(withDashboardParam('/trends/create', 'all')).toBe('/trends/create');
		expect(withDashboardParam('/trends/create', null)).toBe('/trends/create');
	});

	it('computes the views to add and remove', () => {
		expect(viewChanges(['a', 'b'], ['b', 'c'])).toEqual({ add: ['c'], remove: ['a'] });
		expect(viewChanges([], [])).toEqual({ add: [], remove: [] });
	});
});

describe('filters', () => {
	const saved: InsightFilters = filtersOf(
		{ includeRepoRegex: 'acme/', excludeRepoRegex: null, searchContexts: ['@alice/ctx'] },
		{ sortOptions: { mode: 'LEXICOGRAPHICAL', direction: 'ASC' }, limit: 5, numSamples: null }
	);

	it('reads the saved filters, with the default sort when unset', () => {
		expect(saved).toEqual({
			context: '@alice/ctx',
			includeRepoRegexp: 'acme/',
			excludeRepoRegexp: '',
			seriesDisplayOptions: { limit: 5, numSamples: null, sortOptions: { mode: 'LEXICOGRAPHICAL', direction: 'ASC' } }
		});
		const v = view();
		expect(filtersOf(v.defaultFilters, v.defaultSeriesDisplayOptions)).toEqual(EMPTY_FILTERS);
	});

	it('tells active and changed filters', () => {
		expect(hasActiveFilters(EMPTY_FILTERS)).toBe(false);
		expect(hasActiveFilters({ ...EMPTY_FILTERS, excludeRepoRegexp: ' x ' })).toBe(true);
		expect(
			hasActiveFilters({
				...EMPTY_FILTERS,
				seriesDisplayOptions: { ...EMPTY_FILTERS.seriesDisplayOptions, sortOptions: { mode: 'RESULT_COUNT', direction: 'ASC' } }
			})
		).toBe(true);
		expect(filtersEqual(saved, { ...saved, includeRepoRegexp: ' acme/ ' })).toBe(true);
		expect(filtersEqual(saved, { ...saved, context: '' })).toBe(false);
		expect(
			filtersEqual(saved, { ...saved, seriesDisplayOptions: { ...saved.seriesDisplayOptions, limit: null } })
		).toBe(false);
	});

	it('builds the GraphQL inputs', () => {
		expect(toFiltersInput(saved)).toEqual({
			includeRepoRegex: 'acme/',
			excludeRepoRegex: null,
			searchContexts: ['@alice/ctx']
		});
		expect(toFiltersInput(EMPTY_FILTERS)).toEqual({ includeRepoRegex: null, excludeRepoRegex: null, searchContexts: [] });
		expect(toDisplayOptionsInput(saved)).toEqual({
			sortOptions: { mode: 'LEXICOGRAPHICAL', direction: 'ASC' },
			limit: 5,
			numSamples: null
		});
	});

	it('clamps counts and validates regular expressions', () => {
		expect(clampCount('', 20)).toBeNull();
		expect(clampCount('abc', 20)).toBeNull();
		expect(clampCount('0', 20)).toBe(1);
		expect(clampCount('7', 20)).toBe(7);
		expect(clampCount('500', 90)).toBe(90);
		expect(repoRegexError('')).toBeNull();
		expect(repoRegexError('^github\\.com/')).toBeNull();
		expect(repoRegexError('(')).toMatch(/regular expression/);
	});

	it('summarises sections', () => {
		expect(sortAndLimitSummary(saved.seriesDisplayOptions)).toBe('Name A-Z, limit 5');
		expect(sortAndLimitSummary(EMPTY_FILTERS.seriesDisplayOptions)).toBe('Highest result count');
		expect(repositoriesSummary({ ...saved, excludeRepoRegexp: 'test' })).toBe('repo:acme/ -repo:test');
	});

	it('saves filters with every series kept as it is', () => {
		const input = updateFiltersInput(view(), saved);
		expect(input.presentationOptions).toEqual({ title: 'TODOs' });
		expect(input.viewControls.filters.searchContexts).toEqual(['@alice/ctx']);
		expect(input.viewControls.seriesDisplayOptions.limit).toBe(5);
		expect(input.dataSeries).toEqual([
			{
				seriesId: 's1',
				query: 'TODO',
				options: { label: 'TODO', lineColor: '#f00' },
				repositoryScope: { repositories: ['a/b'], repositoryCriteria: null },
				timeScope: { stepInterval: { unit: 'WEEK', value: 2 } },
				generatedFromCaptureGroups: false,
				groupBy: null
			},
			{
				seriesId: 's2',
				query: 'FIXME',
				options: { label: 'FIXME', lineColor: '#0f0' },
				repositoryScope: { repositories: [], repositoryCriteria: 'repo:^x' },
				timeScope: { stepInterval: { unit: 'MONTH', value: 1 } },
				generatedFromCaptureGroups: false,
				groupBy: null
			}
		]);
	});
});

describe('views', () => {
	it('tells trend types', () => {
		const v = view();
		expect(insightType(v)).toBe('search');
		expect(
			insightType({ ...v, dataSeriesDefinitions: [{ ...v.dataSeriesDefinitions[0]!, generatedFromCaptureGroups: true }] })
		).toBe('capture-group');
		expect(insightType({ ...v, dataSeriesDefinitions: [{ ...v.dataSeriesDefinitions[0]!, groupBy: 'REPO' }] })).toBe(
			'compute'
		);
		const pie: InsightViewNode = {
			...v,
			presentation: { __typename: 'PieChartInsightViewPresentation', title: 'x', otherThreshold: 0.03 }
		};
		expect(insightType(pie)).toBe('language-stats');
	});

	it('folds small languages into Other', () => {
		const shares = languageShares(
			[
				{ name: 'Rust', totalLines: 90 },
				{ name: 'Shell', totalLines: 6 },
				{ name: 'Make', totalLines: 4 }
			],
			0.05
		);
		expect(shares).toEqual([
			{ name: 'Rust', lines: 90, share: 0.9 },
			{ name: 'Shell', lines: 6, share: 0.06 },
			{ name: 'Other', lines: 4, share: 0.04 }
		]);
		expect(languageShares([], 0.03)).toEqual([]);
	});
});
