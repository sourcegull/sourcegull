/**
 * Chart models of the trend cards: line charts of search and
 * capture-group trends, bar charts of group-by trends, pie charts of language usage, and
 * the scale, tick, tooltip and arc geometry the hand-rolled SVG charts draw from.
 */
import { searchQueryURL, searchURL } from '#lib/search/query.ts';
import { insightType, languageShares } from './insights.ts';
import type { InsightViewNode } from './queries.ts';

/** Colors of series without one, in this fixed order (the default series colors). */
export const DEFAULT_SERIES_COLORS = [
	'#1c7ed6',
	'#e8590c',
	'#2f9e44',
	'#ae3ec9',
	'#f08c00',
	'#0c8599',
	'#d6336c',
	'#5c940d',
	'#7048e8',
	'#868e96'
];

/** The default color of the `i`th series. */
export function seriesColor(i: number): string {
	return DEFAULT_SERIES_COLORS[i % DEFAULT_SERIES_COLORS.length] ?? '#868e96';
}

/** The chart a view is drawn as. */
export type ChartKind = 'line' | 'bar' | 'pie';

export function chartKind(view: InsightViewNode): ChartKind {
	switch (insightType(view)) {
		case 'language-stats':
			return 'pie';
		case 'compute':
			return 'bar';
		default:
			return 'line';
	}
}

/** The "Start Y Axis at 0" option applies to search trends only. */
export function hasYAxisOption(view: InsightViewNode): boolean {
	return insightType(view) === 'search';
}

/** The data export (`/.api/insights/export/{id}`) covers every type but the language pie. */
export function hasExport(view: InsightViewNode): boolean {
	return insightType(view) !== 'language-stats';
}

export function exportURL(id: string): string {
	return `/.api/insights/export/${encodeURIComponent(id)}`;
}

// ---- Line charts ----------------------------------------------------------------------------------

export interface ChartPoint {
	/** Milliseconds since the epoch. */
	x: number;
	y: number;
	/** The drill-down search of the point, if any. */
	link: string | null;
}

export interface IncompleteAlert {
	__typename: string;
	time: string;
}

export interface LineSeries {
	id: string;
	name: string;
	color: string;
	points: ChartPoint[];
	/** Points flagged incomplete (shown on search trends' legends only). */
	alerts: IncompleteAlert[];
}

/**
 * The drill-down search of a point: its point-in-time query, the series query at
 * `rev:at.time(<point>)`, which shows the results the point counted (for a capture group
 * series the server puts the captured value into it). Links to `pointInTimeQuery` only.
 */
export function pointLink(p: { pointInTimeQuery: string | null }): string | null {
	return p.pointInTimeQuery ? searchQueryURL(p.pointInTimeQuery) : null;
}

export function lineSeries(view: InsightViewNode): LineSeries[] {
	const presentation =
		view.presentation.__typename === 'LineChartInsightViewPresentation' ? view.presentation.seriesPresentation : [];
	const showAlerts = insightType(view) === 'search';
	return view.dataSeries.map((s, i) => {
		const p = presentation.find((c) => c.seriesId === s.seriesId);
		return {
			id: s.seriesId,
			name: p?.label || s.label,
			color: p?.color || seriesColor(i),
			points: s.points
				.map((pt) => ({
					x: Date.parse(pt.dateTime),
					y: pt.value,
					link: pointLink(pt)
				}))
				.filter((pt) => Number.isFinite(pt.x))
				.sort((a, b) => a.x - b.x),
			alerts: showAlerts ? s.status.incompleteDatapoints : []
		};
	});
}

/**
 * The card-level incomplete alert of capture-group and group-by trends: when every series
 * has incomplete points, the first one (search trends show alerts per series instead).
 */
export function topLevelIncompleteAlert(view: InsightViewNode): IncompleteAlert | null {
	if (insightType(view) === 'search' || insightType(view) === 'language-stats') return null;
	const series = view.dataSeries;
	if (series.length === 0 || !series.every((s) => s.status.incompleteDatapoints.length > 0)) return null;
	return series[0]?.status.incompleteDatapoints[0] ?? null;
}

export function incompleteAlertMessage(alert: IncompleteAlert): string {
	return alert.__typename === 'TimeoutDatapointAlert'
		? 'Some points of this data series exceeded the timeout limit. Results may be incomplete.'
		: 'Some points of this data series encountered an error. Results may be incomplete.';
}

/** Whether there is nothing to draw ("No data to display"). */
export function hasNoData(view: InsightViewNode): boolean {
	switch (chartKind(view)) {
		case 'pie':
			return view.languageStatistics.every((l) => l.totalLines === 0);
		case 'bar':
			return barData(view).every((d) => d.value === 0);
		case 'line':
			return view.dataSeries.every((s) => s.points.length === 0);
	}
}

export interface Ticks {
	min: number;
	max: number;
	ticks: number[];
}

function niceStep(span: number, count: number): number {
	const raw = span / Math.max(count, 1);
	const power = 10 ** Math.floor(Math.log10(raw));
	const f = raw / power;
	const nice = f <= 1 ? 1 : f <= 2 ? 2 : f <= 2.5 ? 2.5 : f <= 5 ? 5 : 10;
	return nice * power;
}

/**
 * A "nice" linear domain covering `min…max` with about `count` round ticks. Counts are whole
 * numbers, so steps are at least 1.
 */
export function niceTicks(min: number, max: number, count = 5): Ticks {
	if (!Number.isFinite(min) || !Number.isFinite(max)) return { min: 0, max: 1, ticks: [0, 1] };
	if (min > max) [min, max] = [max, min];
	if (min === max) {
		if (min === 0) return { min: 0, max: 1, ticks: [0, 1] };
		const pad = Math.max(1, Math.abs(min) * 0.1);
		min = min > 0 ? Math.max(0, min - pad) : min - pad;
		max = max + pad;
	}
	const step = Math.max(1, niceStep(max - min, count));
	const lo = Math.floor(min / step) * step;
	const hi = Math.ceil(max / step) * step;
	const ticks: number[] = [];
	for (let v = lo; v <= hi + step / 2; v += step) ticks.push(Math.round(v * 1e6) / 1e6);
	return { min: lo, max: hi, ticks };
}

/** The Y domain of a line chart: from the smallest value, or from 0 ("Start Y Axis at 0"). */
export function yDomain(series: LineSeries[], zeroYAxisMin: boolean, count = 5): Ticks {
	const ys = series.flatMap((s) => s.points.map((p) => p.y));
	if (ys.length === 0) return niceTicks(0, 1, count);
	const max = Math.max(...ys);
	const min = zeroYAxisMin ? Math.min(0, ...ys) : Math.min(...ys);
	return niceTicks(min, max, count);
}

/** Every distinct X of the series, ascending. */
export function xValues(series: LineSeries[]): number[] {
	return [...new Set(series.flatMap((s) => s.points.map((p) => p.x)))].sort((a, b) => a - b);
}

/** At most `max` of `xs`, evenly spread, always with the first and the last. */
export function thinTicks(xs: number[], max: number): number[] {
	if (xs.length <= max) return xs;
	if (max <= 1) return xs.slice(0, 1);
	const out: number[] = [];
	const step = (xs.length - 1) / (max - 1);
	for (let i = 0; i < max; i++) out.push(xs[Math.round(i * step)] ?? 0);
	return [...new Set(out)];
}

const MONTHS = ['Jan', 'Feb', 'Mar', 'Apr', 'May', 'Jun', 'Jul', 'Aug', 'Sep', 'Oct', 'Nov', 'Dec'];
const DAY = 24 * 60 * 60 * 1000;

/** X tick labels: "05 Mar" when the chart spans less than a year, else "Mar 24" (UTC). */
export function dateTickFormatter(xs: number[]): (x: number) => string {
	const span = xs.length > 1 ? (xs.at(-1) ?? 0) - (xs[0] ?? 0) : 0;
	if (span < 365 * DAY) {
		return (x) => {
			const d = new Date(x);
			return `${String(d.getUTCDate()).padStart(2, '0')} ${MONTHS[d.getUTCMonth()]}`;
		};
	}
	return (x) => {
		const d = new Date(x);
		return `${MONTHS[d.getUTCMonth()]} ${String(d.getUTCFullYear() % 100).padStart(2, '0')}`;
	};
}

/** The tooltip's heading, e.g. "Tue Mar 05 2024" (UTC). */
export function tooltipDate(x: number): string {
	const d = new Date(x);
	const days = ['Sun', 'Mon', 'Tue', 'Wed', 'Thu', 'Fri', 'Sat'];
	return `${days[d.getUTCDay()]} ${MONTHS[d.getUTCMonth()]} ${String(d.getUTCDate()).padStart(2, '0')} ${d.getUTCFullYear()}`;
}

const compact = new Intl.NumberFormat('en', {
	notation: 'compact',
	maximumFractionDigits: 1
});

/** Y tick and tooltip values: 950, 1.2K, 3M. */
export function formatValue(v: number): string {
	return Math.abs(v) < 1000 ? String(Math.round(v * 100) / 100) : compact.format(v);
}

/** Index of the value in sorted `xs` nearest to `x`. */
export function nearestIndex(xs: number[], x: number): number {
	if (xs.length === 0) return -1;
	let lo = 0;
	let hi = xs.length - 1;
	while (hi - lo > 1) {
		const mid = (lo + hi) >> 1;
		if ((xs[mid] ?? 0) <= x) lo = mid;
		else hi = mid;
	}
	return Math.abs((xs[hi] ?? 0) - x) < Math.abs((xs[lo] ?? 0) - x) ? hi : lo;
}

/** The series whose point at `x` is nearest to `y` (both in data units). */
export function nearestSeries(series: LineSeries[], x: number, y: number): string | null {
	let best: string | null = null;
	let dist = Infinity;
	for (const s of series) {
		const p = s.points.find((pt) => pt.x === x);
		if (!p) continue;
		const d = Math.abs(p.y - y);
		if (d < dist) {
			dist = d;
			best = s.id;
		}
	}
	return best;
}

export interface TooltipLine {
	id: string;
	name: string;
	color: string;
	value: number;
	link: string | null;
}

export interface ListWindow<T> {
	items: T[];
	before: number;
	after: number;
}

/** At most `size` items around `index`, with the counts left out on each side. */
export function listWindow<T>(list: T[], index: number, size: number): ListWindow<T> {
	if (list.length <= size) return { items: list, before: 0, after: 0 };
	const center = index >= 0 && index < list.length ? index : Math.floor(list.length / 2);
	let start = Math.max(0, center - Math.floor((size - 1) / 2));
	start = Math.min(start, list.length - size);
	return {
		items: list.slice(start, start + size),
		before: start,
		after: list.length - start - size
	};
}

/** The most series a tooltip lists. */
export const MAX_TOOLTIP_ITEMS = 10;

/** The tooltip at `x`: every series with a point there, by value descending, windowed around the active one. */
export function tooltipLines(series: LineSeries[], x: number, activeId: string | null): ListWindow<TooltipLine> {
	const lines = series
		.map((s) => {
			const p = s.points.find((pt) => pt.x === x);
			return p ? { id: s.id, name: s.name, color: s.color, value: p.y, link: p.link } : null;
		})
		.filter((l): l is TooltipLine => l !== null)
		.sort((a, b) => b.value - a.value);
	return listWindow(
		lines,
		lines.findIndex((l) => l.id === activeId),
		MAX_TOOLTIP_ITEMS
	);
}

/** The SVG path of one series: its points joined in X order. */
export function linePath(points: { px: number; py: number }[]): string {
	return points.map((p, i) => `${i === 0 ? 'M' : 'L'}${p.px.toFixed(1)},${p.py.toFixed(1)}`).join('');
}

/**
 * The legend's selection after clicking `id` (the series toggle): selecting every
 * series is the same as selecting none.
 */
export function toggleSeries(selected: string[], id: string, all: string[]): string[] {
	const next = selected.includes(id) ? selected.filter((s) => s !== id) : [...selected, id];
	return next.length === all.length ? [] : next;
}

/** Series shown with a legend selection (none selected: all). */
export function visibleSeries(series: LineSeries[], selected: string[]): LineSeries[] {
	return selected.length === 0 ? series : series.filter((s) => selected.includes(s.id));
}

// ---- Bar charts (group-by trends) ---------------------------------------------------------------

export interface BarDatum {
	name: string;
	value: number;
	color: string;
}

/** Group-by series summed by label, colored as the first series (a categorical chart). */
export function barData(view: InsightViewNode): BarDatum[] {
	const presentation =
		view.presentation.__typename === 'LineChartInsightViewPresentation' ? view.presentation.seriesPresentation : [];
	const color = presentation[0]?.color || seriesColor(0);
	const groups = new Map<string, number>();
	for (const s of view.dataSeries) {
		if (!s.label || s.points.length === 0) continue;
		groups.set(s.label, (groups.get(s.label) ?? 0) + s.points.reduce((n, p) => n + p.value, 0));
	}
	return [...groups].map(([name, value]) => ({ name, value, color }));
}

// ---- Pie charts (language usage) ------------------------------------------------------------------

/** Linguist colors of common languages; others are gray. */
const LANGUAGE_COLORS: Record<string, string> = {
	C: '#555555',
	'C#': '#178600',
	'C++': '#f34b7d',
	CSS: '#563d7c',
	Clojure: '#db5855',
	CoffeeScript: '#244776',
	Dart: '#00b4ab',
	Dockerfile: '#384d54',
	Elixir: '#6e4a7e',
	Erlang: '#b83998',
	Go: '#00add8',
	Groovy: '#4298b8',
	HCL: '#844fba',
	HTML: '#e34c26',
	Haskell: '#5e5086',
	JSON: '#292929',
	Java: '#b07219',
	JavaScript: '#f1e05a',
	Julia: '#a270ba',
	Kotlin: '#a97bff',
	Lua: '#000080',
	Makefile: '#427819',
	Markdown: '#083fa1',
	'Objective-C': '#438eff',
	OCaml: '#ef7a08',
	PHP: '#4f5d95',
	Perl: '#0298c3',
	PowerShell: '#012456',
	Python: '#3572a5',
	R: '#198ce7',
	Ruby: '#701516',
	Rust: '#dea584',
	SCSS: '#c6538c',
	SQL: '#e38c00',
	Scala: '#c22d40',
	Shell: '#89e051',
	Svelte: '#ff3e00',
	Swift: '#f05138',
	TOML: '#9c4221',
	TSX: '#3178c6',
	TypeScript: '#3178c6',
	Vue: '#41b883',
	YAML: '#cb171e',
	Zig: '#ec915c'
};

export function languageColor(name: string): string {
	return LANGUAGE_COLORS[name] ?? 'gray';
}

function escapeRegExp(s: string): string {
	return s.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

function quoteIfNeeded(v: string): string {
	return /\s/.test(v) ? `"${v}"` : v;
}

export interface PieDatum {
	name: string;
	value: number;
	share: number;
	color: string;
	/** A search of the language in the trend's repositories ("Other": all of them). */
	link: string | null;
}

/** Repositories as a `repo:` filter matching exactly them. */
export function repoFilter(repos: string[]): string {
	if (repos.length === 0) return '';
	if (repos.length === 1) return `repo:^${escapeRegExp(repos[0] ?? '')}$`;
	return `repo:^(${repos.map(escapeRegExp).join('|')})$`;
}

/** The repositories of a language pie as a `repo:` filter. */
export function pieRepoFilter(view: InsightViewNode): string {
	const def = view.dataSeriesDefinitions[0]?.repositoryDefinition;
	return repoFilter(def?.__typename === 'InsightRepositoryScope' ? def.repositories : []);
}

export function pieData(view: InsightViewNode): PieDatum[] {
	const threshold =
		view.presentation.__typename === 'PieChartInsightViewPresentation' ? view.presentation.otherThreshold : 0;
	const scope = [pieRepoFilter(view), view.dataSeriesDefinitions[0]?.query.trim() ?? ''].filter(Boolean).join(' ');
	return languagePie(view.languageStatistics, threshold, scope);
}

/**
 * The slices of a language pie: languages under `threshold` summed as "Other", each linked to
 * a search of the language within `scope` (a query of the pie's repositories).
 */
export function languagePie(
	stats: { name: string; totalLines: number }[],
	threshold: number,
	scope: string
): PieDatum[] {
	return languageShares(stats, threshold)
		.map((l) => {
			const q = l.name === 'Other' ? scope : `${scope} lang:${quoteIfNeeded(l.name)}`.trim();
			return {
				name: l.name,
				value: l.lines,
				share: l.share,
				color: l.name === 'Other' ? 'gray' : languageColor(l.name),
				link: q ? searchURL(q, 'standard') : null
			};
		})
		.sort((a, b) => b.value - a.value);
}

export interface PieArc<T> {
	datum: T;
	start: number;
	end: number;
	/** The SVG path of the slice, centred on 0,0. */
	path: string;
	/** The middle angle (radians, 0 = 12 o'clock, clockwise). */
	mid: number;
}

/** Two decimals, without "-0.00". */
function f2(n: number): string {
	const s = n.toFixed(2);
	return s === '-0.00' ? '0.00' : s;
}

function polar(r: number, a: number): [number, number] {
	return [r * Math.sin(a), -r * Math.cos(a)];
}

/** Slices of a pie of radius `r`, clockwise from 12 o'clock, in the data's order. */
export function pieArcs<T extends { value: number }>(data: T[], r: number): PieArc<T>[] {
	const total = data.reduce((n, d) => n + d.value, 0);
	if (total <= 0) return [];
	let a = 0;
	return data.map((datum) => {
		const sweep = (datum.value / total) * Math.PI * 2;
		const start = a;
		const end = a + sweep;
		a = end;
		let path: string;
		if (sweep >= Math.PI * 2 - 1e-9) {
			path = `M0,${-r}A${r},${r} 0 1 1 0,${r}A${r},${r} 0 1 1 0,${-r}Z`;
		} else {
			const [x0, y0] = polar(r, start);
			const [x1, y1] = polar(r, end);
			const large = sweep > Math.PI ? 1 : 0;
			path = `M0,0L${f2(x0)},${f2(y0)}A${r},${r} 0 ${large} 1 ${f2(x1)},${f2(y1)}Z`;
		}
		return { datum, start, end, path, mid: (start + end) / 2 };
	});
}

export interface PieLabel {
	/** The leader line from the rim out, then sideways. */
	line: string;
	x: number;
	y: number;
	anchor: 'start' | 'end';
}

/** Vertical room of a two-line label (name and share). */
export const PIE_LABEL_HEIGHT = 28;

/**
 * Where the slices' labels go: beside the pie at their slice's middle angle, pushed apart
 * vertically on each side so that labels of small neighbouring slices do not overlap.
 */
export function pieLabels(mids: number[], r: number, gap = PIE_LABEL_HEIGHT): PieLabel[] {
	const ideal = mids.map((mid) => {
		const [, y1] = polar(r * 1.18, mid);
		return { mid, right: Math.sin(mid) >= 0, y: y1 };
	});
	const ys = ideal.map((l) => l.y);
	for (const right of [true, false]) {
		const side = ideal.map((l, i) => ({ ...l, i })).filter((l) => l.right === right);
		side.sort((a, b) => a.y - b.y);
		for (let k = 1; k < side.length; k++) {
			const prev = ys[side[k - 1]!.i]!;
			const cur = side[k]!;
			ys[cur.i] = Math.max(ys[cur.i]!, prev + gap);
		}
		// Pushed below the pie's bottom: shift the column back up, keeping the spacing.
		const last = side.at(-1);
		const overflow = last ? ys[last.i]! - (r * 1.25 + gap / 2) : 0;
		if (overflow > 0) {
			for (let k = side.length - 1; k >= 0; k--) {
				const cur = side[k]!;
				const below = k + 1 < side.length ? ys[side[k + 1]!.i]! - gap : Infinity;
				ys[cur.i] = Math.min(ys[cur.i]! - overflow, below);
			}
		}
	}
	return ideal.map((l, i) => {
		const [x0, y0] = polar(r, l.mid);
		const [x1] = polar(r * 1.18, l.mid);
		const y = ys[i]!;
		const x2 = x1 + (l.right ? 12 : -12);
		return {
			line: `M${x0.toFixed(1)},${y0.toFixed(1)}L${x1.toFixed(1)},${y.toFixed(1)}L${x2.toFixed(1)},${y.toFixed(1)}`,
			x: x2 + (l.right ? 4 : -4),
			y,
			anchor: l.right ? 'start' : 'end'
		};
	});
}
