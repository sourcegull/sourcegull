import { describe, expect, it } from 'vitest';
import {
	barData,
	chartKind,
	dateTickFormatter,
	exportURL,
	formatValue,
	hasExport,
	hasNoData,
	hasYAxisOption,
	lineSeries,
	linePath,
	listWindow,
	nearestIndex,
	nearestSeries,
	niceTicks,
	pieArcs,
	pieData,
	pieLabels,
	pointLink,
	thinTicks,
	toggleSeries,
	tooltipDate,
	tooltipLines,
	topLevelIncompleteAlert,
	visibleSeries,
	xValues,
	yDomain,
	type LineSeries
} from './charts.ts';
import type { DataSeries, InsightViewNode, SeriesDefinition } from './queries.ts';

const JAN = Date.parse('2026-01-01T00:00:00Z');
const FEB = Date.parse('2026-02-01T00:00:00Z');
const MAR = Date.parse('2026-03-01T00:00:00Z');

function def(overrides: Partial<SeriesDefinition> = {}): SeriesDefinition {
	return {
		__typename: 'SearchInsightDataSeriesDefinition',
		seriesId: 's1',
		query: 'TODO',
		repositoryDefinition: {
			__typename: 'InsightRepositoryScope',
			repositories: ['gitlab.example.com/acme/sourcegull']
		},
		timeScope: {
			__typename: 'InsightIntervalTimeScope',
			unit: 'MONTH',
			value: 1
		},
		generatedFromCaptureGroups: false,
		isCalculated: false,
		groupBy: null,
		...overrides
	};
}

function series(id: string, label: string, values: [string, number][], incomplete = 0): DataSeries {
	return {
		seriesId: id,
		label,
		points: values.map(([dateTime, value], i) => ({
			dateTime,
			value,
			diffQuery: `${label} type:diff after:${i} before:${dateTime}`,
			pointInTimeQuery: `${label} rev:at.time(${dateTime})`
		})),
		status: {
			isLoadingData: false,
			pendingJobs: 0,
			completedJobs: 1,
			failedJobs: 0,
			backfillQueuedAt: null,
			incompleteDatapoints: Array.from({ length: incomplete }, () => ({
				__typename: 'TimeoutDatapointAlert',
				time: '2026-02-01T00:00:00Z'
			}))
		}
	};
}

function view(overrides: Partial<InsightViewNode> = {}): InsightViewNode {
	const display = {
		sortOptions: { mode: null, direction: null },
		limit: null,
		numSamples: null
	};
	const filters = {
		includeRepoRegex: null,
		excludeRepoRegex: null,
		searchContexts: null
	};
	return {
		id: 'V1',
		isFrozen: false,
		dashboardReferenceCount: 1,
		seriesCount: 2,
		presentation: {
			__typename: 'LineChartInsightViewPresentation',
			title: 'TODOs',
			seriesPresentation: [{ seriesId: 's1', label: 'TODO label', color: '#f00' }]
		},
		defaultFilters: filters,
		appliedFilters: filters,
		defaultSeriesDisplayOptions: display,
		appliedSeriesDisplayOptions: display,
		dataSeriesDefinitions: [def(), def({ seriesId: 's2', query: 'FIXME' })],
		dataSeries: [
			series('s1', 'TODO', [
				['2026-02-01T00:00:00Z', 5],
				['2026-01-01T00:00:00Z', 3]
			]),
			series('s2', 'FIXME', [['2026-01-01T00:00:00Z', 7]], 1)
		],
		languageStatistics: [],
		...overrides
	};
}

const pie = (stats: { name: string; totalLines: number }[], repos = ['gitlab.example.com/acme/sourcegull']) =>
	view({
		presentation: {
			__typename: 'PieChartInsightViewPresentation',
			title: 'Langs',
			otherThreshold: 0.05
		},
		dataSeriesDefinitions: [
			def({
				query: '',
				repositoryDefinition: {
					__typename: 'InsightRepositoryScope',
					repositories: repos
				}
			})
		],
		dataSeries: [],
		languageStatistics: stats
	});

const groupBy = () =>
	view({
		dataSeriesDefinitions: [def({ groupBy: 'REPO' })],
		dataSeries: [
			series('a', 'acme/a', [['2026-01-01T00:00:00Z', 2]]),
			series('b', 'acme/b', [['2026-01-01T00:00:00Z', 4]]),
			series('c', 'acme/a', [['2026-01-01T00:00:00Z', 1]])
		]
	});

describe('chart kinds and menu options', () => {
	it('draws search and capture groups as lines, group-by as bars, languages as a pie', () => {
		expect(chartKind(view())).toBe('line');
		expect(
			chartKind(
				view({
					dataSeriesDefinitions: [def({ generatedFromCaptureGroups: true })]
				})
			)
		).toBe('line');
		expect(chartKind(groupBy())).toBe('bar');
		expect(chartKind(pie([]))).toBe('pie');
	});

	it('offers the Y axis option on search trends and the export on all but pies', () => {
		expect(hasYAxisOption(view())).toBe(true);
		expect(
			hasYAxisOption(
				view({
					dataSeriesDefinitions: [def({ generatedFromCaptureGroups: true })]
				})
			)
		).toBe(false);
		expect(hasYAxisOption(pie([]))).toBe(false);
		expect(hasExport(view())).toBe(true);
		expect(hasExport(groupBy())).toBe(true);
		expect(hasExport(pie([]))).toBe(false);
		expect(exportURL('aW5zaWdodA==')).toBe('/.api/insights/export/aW5zaWdodA%3D%3D');
	});
});

describe('line series', () => {
	it('sorts points, takes presentation labels and colors, and links points to their point-in-time search', () => {
		const [a, b] = lineSeries(view()) as [LineSeries, LineSeries];
		expect(a.name).toBe('TODO label');
		expect(a.color).toBe('#f00');
		expect(a.points.map((p) => [p.x, p.y])).toEqual([
			[JAN, 3],
			[FEB, 5]
		]);
		expect(a.points[1]!.link).toBe('/search?q=TODO+rev%3Aat.time%282026-02-01T00%3A00%3A00Z%29');
		// No presentation: the series label and the first default color after its index.
		expect(b.name).toBe('FIXME');
		expect(b.color).toMatch(/^#/);
		expect(b.alerts).toHaveLength(1);
	});

	it('links the point-in-time search, not the diff search, and keeps a patternType as a parameter', () => {
		expect(pointLink({ pointInTimeQuery: 'x rev:at.time(2026)' })).toBe('/search?q=x+rev%3Aat.time%282026%29');
		expect(pointLink({ pointInTimeQuery: 'a rev:at.time(2026) patternType:regexp' })).toBe(
			'/search?q=a+rev%3Aat.time%282026%29&patternType=regexp'
		);
		expect(pointLink({ pointInTimeQuery: null })).toBeNull();
	});

	it('shows incomplete alerts per series on search trends only', () => {
		expect(
			lineSeries(
				view({
					dataSeriesDefinitions: [def({ generatedFromCaptureGroups: true })]
				})
			)[1]!.alerts
		).toEqual([]);
		expect(topLevelIncompleteAlert(view())).toBeNull();
		const capture = view({
			dataSeriesDefinitions: [def({ generatedFromCaptureGroups: true })],
			dataSeries: [series('a', 'a', [], 1), series('b', 'b', [], 2)]
		});
		expect(topLevelIncompleteAlert(capture)?.__typename).toBe('TimeoutDatapointAlert');
		expect(
			topLevelIncompleteAlert(
				view({
					dataSeriesDefinitions: [def({ generatedFromCaptureGroups: true })],
					dataSeries: [series('a', 'a', [], 1), series('b', 'b', [])]
				})
			)
		).toBeNull();
	});

	it('knows when there is nothing to draw', () => {
		expect(hasNoData(view())).toBe(false);
		expect(hasNoData(view({ dataSeries: [series('a', 'a', [])] }))).toBe(true);
		expect(
			hasNoData(
				view({
					dataSeriesDefinitions: [def({ groupBy: 'REPO' })],
					dataSeries: [series('a', 'a', [['2026-01-01T00:00:00Z', 0]])]
				})
			)
		).toBe(true);
		expect(hasNoData(pie([]))).toBe(true);
		expect(hasNoData(pie([{ name: 'Go', totalLines: 3 }]))).toBe(false);
	});
});

describe('scales and ticks', () => {
	it('rounds domains to nice steps of whole numbers', () => {
		expect(niceTicks(3, 97, 5)).toEqual({
			min: 0,
			max: 100,
			ticks: [0, 20, 40, 60, 80, 100]
		});
		expect(niceTicks(0, 3, 5).ticks).toEqual([0, 1, 2, 3]);
		expect(niceTicks(0, 0)).toEqual({ min: 0, max: 1, ticks: [0, 1] });
		const flat = niceTicks(50, 50);
		expect(flat.min).toBeLessThan(50);
		expect(flat.max).toBeGreaterThan(50);
	});

	it('starts the Y axis at the smallest value unless asked to start at 0', () => {
		const s: LineSeries[] = [
			{
				id: 'a',
				name: 'a',
				color: '#000',
				alerts: [],
				points: [
					{ x: 1, y: 120, link: null },
					{ x: 2, y: 180, link: null }
				]
			}
		];
		expect(yDomain(s, false).min).toBe(120);
		expect(yDomain(s, true).min).toBe(0);
		expect(yDomain([], false)).toEqual({ min: 0, max: 1, ticks: [0, 1] });
	});

	it('thins X ticks keeping the ends', () => {
		expect(thinTicks([1, 2, 3], 5)).toEqual([1, 2, 3]);
		expect(thinTicks([1, 2, 3, 4, 5, 6, 7, 8, 9], 3)).toEqual([1, 5, 9]);
	});

	it('formats dates by span and values compactly', () => {
		expect(dateTickFormatter([JAN, MAR])(FEB)).toBe('01 Feb');
		expect(dateTickFormatter([JAN, JAN + 400 * 86400000])(FEB)).toBe('Feb 26');
		expect(tooltipDate(FEB)).toBe('Sun Feb 01 2026');
		expect(formatValue(950)).toBe('950');
		expect(formatValue(1234)).toBe('1.2K');
		expect(formatValue(2.5)).toBe('2.5');
	});

	it('draws a path through points', () => {
		expect(
			linePath([
				{ px: 0, py: 10 },
				{ px: 5.25, py: 2 }
			])
		).toBe('M0.0,10.0L5.3,2.0');
	});
});

describe('hover and tooltip', () => {
	const s = lineSeries(view());

	it('finds the nearest date and series', () => {
		const xs = xValues(s);
		expect(xs).toEqual([JAN, FEB]);
		expect(nearestIndex(xs, JAN + 1000)).toBe(0);
		expect(nearestIndex(xs, FEB - 1000)).toBe(1);
		expect(nearestIndex([], 5)).toBe(-1);
		expect(nearestSeries(s, JAN, 6)).toBe('s2');
		expect(nearestSeries(s, JAN, 2)).toBe('s1');
		expect(nearestSeries(s, MAR, 2)).toBeNull();
	});

	it('lists series at a date by value with their links', () => {
		const t = tooltipLines(s, JAN, 's1');
		expect(t.items.map((l) => [l.name, l.value])).toEqual([
			['FIXME', 7],
			['TODO label', 3]
		]);
		expect(t.items[1]!.link).toContain('rev%3Aat.time');
		expect(tooltipLines(s, FEB, null).items).toHaveLength(1);
	});

	it('windows long lists around the active item', () => {
		const list = [1, 2, 3, 4, 5, 6, 7];
		expect(listWindow(list, 3, 3)).toEqual({
			items: [3, 4, 5],
			before: 2,
			after: 2
		});
		expect(listWindow(list, 0, 3)).toEqual({
			items: [1, 2, 3],
			before: 0,
			after: 4
		});
		expect(listWindow(list, 6, 3)).toEqual({
			items: [5, 6, 7],
			before: 4,
			after: 0
		});
		expect(listWindow(list, -1, 3).items).toEqual([3, 4, 5]);
		expect(listWindow([1, 2], 0, 3)).toEqual({
			items: [1, 2],
			before: 0,
			after: 0
		});
	});

	it('toggles legend selection, selecting all meaning none', () => {
		expect(toggleSeries([], 'a', ['a', 'b', 'c'])).toEqual(['a']);
		expect(toggleSeries(['a'], 'a', ['a', 'b', 'c'])).toEqual([]);
		expect(toggleSeries(['a', 'b'], 'c', ['a', 'b', 'c'])).toEqual([]);
		expect(visibleSeries(s, []).length).toBe(2);
		expect(visibleSeries(s, ['s2']).map((x) => x.id)).toEqual(['s2']);
	});
});

describe('bars and pies', () => {
	it('sums group-by series by label in the first series color', () => {
		expect(barData(groupBy())).toEqual([
			{ name: 'acme/a', value: 3, color: '#f00' },
			{ name: 'acme/b', value: 4, color: '#f00' }
		]);
	});

	it('builds language slices with Other and search links', () => {
		const d = pieData(
			pie([
				{ name: 'Rust', totalLines: 80 },
				{ name: 'Shell', totalLines: 2 },
				{ name: 'Go Template', totalLines: 18 }
			])
		);
		expect(d.map((x) => [x.name, x.value])).toEqual([
			['Rust', 80],
			['Go Template', 18],
			['Other', 2]
		]);
		expect(d[0]!.color).toBe('#dea584');
		expect(d[2]!.color).toBe('gray');
		const q = (url: string | null) => new URLSearchParams(url!.split('?')[1]).get('q');
		expect(q(d[0]!.link)).toBe('repo:^gitlab\\.example\\.com/acme/sourcegull$ lang:Rust');
		expect(q(d[1]!.link)).toBe('repo:^gitlab\\.example\\.com/acme/sourcegull$ lang:"Go Template"');
		expect(q(d[2]!.link)).toBe('repo:^gitlab\\.example\\.com/acme/sourcegull$');
		const many = pieData(pie([{ name: 'Go', totalLines: 1 }], ['a/b', 'c/d']));
		expect(q(many[0]!.link)).toBe('repo:^(a/b|c/d)$ lang:Go');
	});

	it('cuts arcs clockwise from 12 o’clock', () => {
		const arcs = pieArcs([{ value: 1 }, { value: 3 }], 10);
		expect(arcs[0]!.start).toBe(0);
		expect(arcs[0]!.end).toBeCloseTo(Math.PI / 2);
		expect(arcs[1]!.end).toBeCloseTo(Math.PI * 2);
		expect(arcs[0]!.path).toBe('M0,0L0.00,-10.00A10,10 0 0 1 10.00,0.00Z');
		expect(arcs[1]!.path).toContain(' 0 1 1 ');
		expect(pieArcs([{ value: 5 }], 10)[0]!.path).toContain('A10,10 0 1 1 0,10');
		expect(pieArcs([{ value: 0 }], 10)).toEqual([]);
		const [right, left] = pieLabels([Math.PI / 2, (3 * Math.PI) / 2], 10);
		expect(right?.anchor).toBe('start');
		expect(left?.anchor).toBe('end');
	});

	it('pushes labels of neighbouring small slices apart', () => {
		const labels = pieLabels([0.1, 0.15, 0.2, Math.PI], 100, 28);
		const ys = labels.map((l) => l.y);
		expect(ys[1]! - ys[0]!).toBeGreaterThanOrEqual(28);
		expect(ys[2]! - ys[1]!).toBeGreaterThanOrEqual(28);
		// The lone label on the other side stays where its slice is.
		expect(ys[3]).toBeCloseTo(118);
		// A crowded column never runs far below the pie.
		const crowded = pieLabels([2.9, 3.0, 3.05, 3.1], 100, 28).map((l) => l.y);
		expect(Math.max(...crowded)).toBeLessThanOrEqual(125 + 14 + 1e-9);
	});
});
