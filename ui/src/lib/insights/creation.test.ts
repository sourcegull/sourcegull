import { describe, expect, it } from 'vitest';
import {
	captureGroupCreateInput,
	captureGroupFormErrors,
	captureGroupQueryError,
	captureGroupUpdateInput,
	computeCreateInput,
	computeFormErrors,
	computePreviewInput,
	creationType,
	creationURL,
	defaultCaptureGroupFields,
	defaultComputeFields,
	defaultLangStatsFields,
	defaultSearchFields,
	editFormOf,
	formatDuration,
	groupByUnavailable,
	hasCaptureGroup,
	langStatsCreateInput,
	langStatsFormErrors,
	newSeries,
	previewBarData,
	previewLineSeries,
	previewModelEmpty,
	previewPieData,
	queryChecks,
	repositoryScopeInput,
	scanQuery,
	searchCreateInput,
	searchFormErrors,
	searchPreviewInput,
	searchUpdateInput,
	seriesListError,
	seriesQueryError,
	stepDescription,
	stepValueError,
	thresholdError,
	viewControlsOf,
	type SearchInsightFields
} from './creation.ts';
import type { InsightViewNode, SeriesDefinition } from './queries.ts';

function searchFields(over: Partial<SearchInsightFields> = {}): SearchInsightFields {
	return {
		...defaultSearchFields(),
		title: 'Migration',
		repoMode: 'urls-list',
		repositories: ['gitlab.com/a/b'],
		series: [newSeries(0, { name: 'TODO', query: 'TODO', stroke: '#e8590c' })],
		...over
	};
}

function def(over: Partial<SeriesDefinition> = {}): SeriesDefinition {
	return {
		__typename: 'SearchInsightDataSeriesDefinition',
		seriesId: 's1',
		query: 'TODO',
		repositoryDefinition: { __typename: 'InsightRepositoryScope', repositories: ['gitlab.com/a/b'] },
		timeScope: { __typename: 'InsightIntervalTimeScope', unit: 'WEEK', value: 2 },
		generatedFromCaptureGroups: false,
		isCalculated: false,
		groupBy: null,
		...over
	};
}

function view(over: Partial<InsightViewNode> = {}): InsightViewNode {
	const display = { sortOptions: { mode: 'LEXICOGRAPHICAL', direction: 'ASC' }, limit: 5, numSamples: null } as const;
	return {
		id: 'v1',
		isFrozen: false,
		dashboardReferenceCount: 1,
		seriesCount: 1,
		presentation: {
			__typename: 'LineChartInsightViewPresentation',
			title: 'TODOs',
			seriesPresentation: [{ seriesId: 's1', label: 'TODO', color: '#f00' }]
		},
		defaultFilters: { includeRepoRegex: 'a/', excludeRepoRegex: null, searchContexts: [] },
		appliedFilters: { includeRepoRegex: null, excludeRepoRegex: null, searchContexts: null },
		defaultSeriesDisplayOptions: { ...display, sortOptions: { ...display.sortOptions } },
		appliedSeriesDisplayOptions: { ...display, sortOptions: { ...display.sortOptions } },
		dataSeriesDefinitions: [def()],
		dataSeries: [],
		languageStatistics: [],
		...over
	};
}

describe('routes', () => {
	it('maps types to creation paths and back', () => {
		expect(creationURL('compute', null)).toBe('/trends/create/group-results');
		expect(creationURL('language-stats', 'd 1')).toBe('/trends/create/lang-stats?dashboardId=d%201');
		expect(creationURL(null, 'all')).toBe('/trends/create');
		expect(creationType('capture-group')).toBe('capture-group');
		expect(creationType('search')).toBe('search');
		expect(creationType('nope')).toBeNull();
	});
});

describe('query scanning', () => {
	it('finds filters, aliases, negation and operators', () => {
		const q = scanQuery('r:foo -repo:"bar baz" TODO and lang:go patternType:regexp');
		expect(q.filters).toEqual([
			{ field: 'repo', value: 'foo', negated: false },
			{ field: 'repo', value: 'bar baz', negated: true },
			{ field: 'lang', value: 'go', negated: false },
			{ field: 'patterntype', value: 'regexp', negated: false }
		]);
		expect(q.operators).toEqual(['AND']);
		expect(q.patterns).toEqual(['TODO']);
	});

	it('keeps quoted text whole', () => {
		expect(scanQuery('"repo:x y" z').patterns).toEqual(['"repo:x y"', 'z']);
	});

	it('detects capture groups', () => {
		expect(hasCaptureGroup('TODO\\((\\w+)\\)')).toBe(true);
		expect(hasCaptureGroup('(?P<name>\\w+)')).toBe(true);
		expect(hasCaptureGroup('(?:abc)')).toBe(false);
		expect(hasCaptureGroup('\\(abc\\)')).toBe(false);
		expect(hasCaptureGroup('[(]abc')).toBe(false);
	});
});

describe('validation', () => {
	it('reports series query errors', () => {
		expect(seriesQueryError('')).toBe('Query is a required field for data series.');
		expect(seriesQueryError('TODO context:global')).toMatch(/^The `context:` filter is not supported/);
		expect(seriesQueryError('TODO repo:a')).toMatch(/^Do not include a `repo:` filter/);
		expect(seriesQueryError('TODO rev:main')).toBe('The `rev:` filter is not currently supported.');
		expect(seriesQueryError('TODO lang:go')).toBeNull();
	});

	it('checks capture group queries', () => {
		expect(captureGroupQueryError('')).toBe('Query is a required field.');
		expect(captureGroupQueryError('TODO')).toBe('Query is not valid');
		expect(captureGroupQueryError('TODO\\((\\w+)\\)')).toBeNull();
		expect(captureGroupQueryError('(\\w+) repo:a')).toBe('Query is not valid');
		const c = queryChecks('(\\w+) type:diff OR x patterntype:literal rev:main');
		expect(c).toMatchObject({
			hasCaptureGroup: true,
			isValidOperator: false,
			isValidPatternType: false,
			isNotRepo: true,
			isNotContext: true,
			isNotCommitOrDiff: false,
			isNotRev: false
		});
		expect(Object.values(queryChecks('  ')).every((v) => v === undefined)).toBe(true);
	});

	it('validates the series list, step and threshold', () => {
		expect(seriesListError([])).toMatch(/^No series defined/);
		expect(seriesListError([newSeries(0)])).toBe('Some series is invalid. Remove or edit the invalid series.');
		expect(stepValueError('')).toBe('Please specify a step between points.');
		expect(stepValueError('0')).not.toBeNull();
		expect(stepValueError('1.5')).not.toBeNull();
		expect(stepValueError('3')).toBeNull();
		expect(thresholdError('0')).not.toBeNull();
		expect(thresholdError('3')).toBeNull();
	});

	it('collects the errors of each form', () => {
		expect(searchFormErrors(searchFields())).toEqual({});
		expect(Object.keys(searchFormErrors(defaultSearchFields())).sort()).toEqual([
			'repositories',
			'series',
			'title'
		]);
		expect(searchFormErrors(searchFields({ repoMode: 'all-repos', repositories: [] }))).toEqual({});
		expect(searchFormErrors(searchFields({ repoMode: 'search-query', repoQuery: '' }))).toEqual({
			repositories: 'Search repositories query is a required field, please fill in the field.'
		});
		expect(Object.keys(captureGroupFormErrors(defaultCaptureGroupFields())).sort()).toEqual([
			'query',
			'repositories',
			'title'
		]);
		expect(computeFormErrors(defaultComputeFields()).repositories).toBe('Repositories is a required field.');
		expect(langStatsFormErrors(defaultLangStatsFields())).toEqual({
			repository: 'Repositories is a required field.',
			title: 'Title is a required field.'
		});
	});
});

describe('group by', () => {
	it('needs commit searches for author and date, content searches for lang and path', () => {
		const content = [{ query: 'TODO' }];
		const diff = [{ query: 'type:diff TODO' }];
		expect(groupByUnavailable('REPO', content)).toBeNull();
		expect(groupByUnavailable('AUTHOR', content)).toMatch(/type:commit and type:diff/);
		expect(groupByUnavailable('DATE', diff)).toBeNull();
		expect(groupByUnavailable('LANG', diff)).not.toBeNull();
		expect(groupByUnavailable('PATH', content)).toBeNull();
	});
});

describe('step description', () => {
	it('formats spans in the largest units', () => {
		expect(formatDuration(29, 'MONTH')).toBe('2 years and 5 months');
		expect(formatDuration(1, 'DAY')).toBe('1 day');
		expect(formatDuration(3, 'WEEK')).toBe('3 weeks');
		expect(stepDescription('1', 'MONTH', 12)).toBe(
			'Show the past 11 months of data, one datapoint every month. This trend provides 12 datapoints.'
		);
		expect(stepDescription('2', 'WEEK', 30)).toBe(
			'Show the past 11 months and 3 weeks of data, one datapoint every 2 weeks. This trend provides 30 datapoints.'
		);
		expect(stepDescription('', 'DAY')).toBe('');
	});
});

describe('inputs', () => {
	it('sends the repository scope of each mode', () => {
		const base = { repoQuery: 'repo:a', repositories: ['x'] };
		expect(repositoryScopeInput({ ...base, repoMode: 'search-query' })).toEqual({
			repositories: [],
			repositoryCriteria: 'repo:a'
		});
		expect(repositoryScopeInput({ ...base, repoMode: 'urls-list' })).toEqual({
			repositories: ['x'],
			repositoryCriteria: null
		});
		expect(repositoryScopeInput({ ...base, repoMode: 'all-repos' })).toEqual({
			repositories: [],
			repositoryCriteria: null
		});
	});

	it('creates a search trend on the dashboard it came from', () => {
		const input = searchCreateInput(searchFields({ stepUnit: 'WEEK', stepValue: '2', title: ' T ' }), 'd1');
		expect(input).toEqual({
			dataSeries: [
				{
					seriesId: null,
					query: 'TODO',
					options: { label: 'TODO', lineColor: '#e8590c' },
					timeScope: { stepInterval: { unit: 'WEEK', value: 2 } },
					generatedFromCaptureGroups: false,
					groupBy: null
				}
			],
			repositoryScope: { repositories: ['gitlab.com/a/b'], repositoryCriteria: null },
			options: { title: 'T' },
			viewControls: expect.objectContaining({ filters: expect.any(Object) }),
			dashboards: ['d1']
		});
		expect(searchCreateInput(searchFields(), null).dashboards).toBeUndefined();
	});

	it('creates capture group, group results and language usage trends', () => {
		const cg = captureGroupCreateInput({ ...defaultCaptureGroupFields(), title: 'V', query: '(\\d+)', repoQuery: 'repo:a' }, null);
		expect(cg.dataSeries[0]).toMatchObject({ generatedFromCaptureGroups: true, groupBy: null, query: '(\\d+)' });
		expect(cg.dataSeries[0]?.timeScope).toEqual({ stepInterval: { unit: 'MONTH', value: 2 } });
		expect(cg.repositoryScope).toEqual({ repositories: [], repositoryCriteria: 'repo:a' });

		const compute = computeCreateInput(
			{ title: 'G', repositories: ['r'], series: [newSeries(0, { name: 'a', query: 'TODO' })], groupBy: 'LANG' },
			null
		);
		expect(compute.dataSeries[0]).toMatchObject({
			groupBy: 'LANG',
			generatedFromCaptureGroups: true,
			repositoryScope: { repositories: ['r'], repositoryCriteria: null },
			timeScope: { stepInterval: { unit: 'WEEK', value: 2 } }
		});

		expect(langStatsCreateInput({ title: 'L', repository: ' r ', threshold: '5' }, 'd')).toEqual({
			query: '',
			repositoryScope: { repositories: ['r'], repositoryCriteria: null },
			presentationOptions: { title: 'L', otherThreshold: 0.05 },
			dashboards: ['d']
		});
	});

	it('keeps series ids and the saved view controls on updates', () => {
		const v = view();
		const form = editFormOf(v);
		if (form.type !== 'search') throw new Error(form.type);
		const input = searchUpdateInput({ ...form.fields, title: 'New' }, viewControlsOf(v));
		expect(input.dataSeries[0]?.seriesId).toBe('s1');
		expect(input.presentationOptions.title).toBe('New');
		expect(input.viewControls).toEqual({
			filters: { includeRepoRegex: 'a/', excludeRepoRegex: null, searchContexts: [] },
			seriesDisplayOptions: { sortOptions: { mode: 'LEXICOGRAPHICAL', direction: 'ASC' }, limit: 5, numSamples: null }
		});
		const cg = captureGroupUpdateInput(defaultCaptureGroupFields(), 's9', viewControlsOf(v));
		expect(cg.dataSeries[0]?.seriesId).toBe('s9');
	});
});

describe('edit forms', () => {
	it('loads a search trend', () => {
		const form = editFormOf(view());
		expect(form.type).toBe('search');
		if (form.type !== 'search') return;
		expect(form.fields).toMatchObject({
			title: 'TODOs',
			repoMode: 'urls-list',
			repositories: ['gitlab.com/a/b'],
			stepUnit: 'WEEK',
			stepValue: '2'
		});
		expect(form.fields.series[0]).toMatchObject({ seriesId: 's1', name: 'TODO', query: 'TODO', stroke: '#f00' });
	});

	it('loads repository queries and all repositories', () => {
		const q = editFormOf(
			view({
				dataSeriesDefinitions: [
					def({
						generatedFromCaptureGroups: true,
						repositoryDefinition: { __typename: 'RepositorySearchScope', search: 'repo:a', allRepositories: false }
					})
				]
			})
		);
		expect(q.type).toBe('capture-group');
		if (q.type !== 'capture-group') return;
		expect(q.seriesId).toBe('s1');
		expect(q.fields).toMatchObject({ repoMode: 'search-query', repoQuery: 'repo:a', query: 'TODO' });
		const all = editFormOf(
			view({
				dataSeriesDefinitions: [
					def({ repositoryDefinition: { __typename: 'RepositorySearchScope', search: '', allRepositories: true } })
				]
			})
		);
		expect(all.type === 'search' && all.fields.repoMode).toBe('all-repos');
	});

	it('loads group results and language usage trends', () => {
		const compute = editFormOf(view({ dataSeriesDefinitions: [def({ groupBy: 'AUTHOR', generatedFromCaptureGroups: true })] }));
		expect(compute.type).toBe('compute');
		if (compute.type === 'compute') expect(compute.fields).toMatchObject({ groupBy: 'AUTHOR', repositories: ['gitlab.com/a/b'] });
		const pie = editFormOf(
			view({ presentation: { __typename: 'PieChartInsightViewPresentation', title: 'Langs', otherThreshold: 0.03 } })
		);
		expect(pie).toEqual({ type: 'language-stats', fields: { title: 'Langs', repository: 'gitlab.com/a/b', threshold: '3' } });
	});
});

describe('previews', () => {
	const points = (values: number[]) =>
		values.map((value, i) => ({
			dateTime: `2026-0${i + 1}-01T00:00:00Z`,
			value,
			diffQuery: 'type:diff TODO',
			pointInTimeQuery: `TODO rev:at.time(2026-0${i + 1}-01T00:00:00Z)`
		}));

	it('previews only valid series over the form scope', () => {
		const f = searchFields({ series: [newSeries(0, { name: 'a', query: 'A' }), newSeries(1)] });
		expect(searchPreviewInput(f)).toEqual({
			repositoryScope: { repositories: ['gitlab.com/a/b'], repositoryCriteria: null },
			timeScope: { stepInterval: { unit: 'MONTH', value: 1 } },
			series: [{ query: 'A', label: 'a', generatedFromCaptureGroups: false, groupBy: null }]
		});
		const c = computePreviewInput({ title: '', repositories: ['r'], series: [newSeries(0, { name: 'n', query: 'q' })], groupBy: 'PATH' });
		expect(c.series[0]).toMatchObject({ groupBy: 'PATH', generatedFromCaptureGroups: true });
	});

	it('colors line series from the form, generated ones by default', () => {
		const s = previewLineSeries(
			[
				{ label: 'a', points: points([1, 2]) },
				{ label: '1.2', points: points([3]) }
			],
			[{ label: 'a', stroke: '#123456' }]
		);
		expect(s.map((x) => [x.name, x.color])).toEqual([
			['a', '#123456'],
			['1.2', '#e8590c']
		]);
		expect(s[0]?.points[1]).toMatchObject({ y: 2, link: expect.stringContaining('rev%3Aat.time') });
		expect(previewLineSeries([{ label: 'x', points: [] }], [], 1)).toHaveLength(1);
		expect(previewModelEmpty({ kind: 'line', series: previewLineSeries([{ label: 'x', points: [] }], []) })).toBe(true);
	});

	it('sums group-by series into bars', () => {
		expect(
			previewBarData(
				[
					{ label: 'a', points: points([1, 2]) },
					{ label: 'a', points: points([3]) },
					{ label: '', points: points([9]) },
					{ label: 'b', points: points([0]) }
				],
				'#fff'
			)
		).toEqual([
			{ name: 'a', value: 6, color: '#fff' },
			{ name: 'b', value: 0, color: '#fff' }
		]);
	});

	it('draws a language pie with an Other slice', () => {
		const d = previewPieData(
			[
				{ name: 'Go', totalLines: 97 },
				{ name: 'Shell', totalLines: 2 },
				{ name: 'Make', totalLines: 1 }
			],
			'gitlab.com/a/b',
			'3'
		);
		expect(d.map((x) => [x.name, x.value])).toEqual([
			['Go', 97],
			['Other', 3]
		]);
		expect(d[0]?.link).toContain('lang%3AGo');
		expect(previewModelEmpty({ kind: 'pie', data: [] })).toBe(true);
	});
});
