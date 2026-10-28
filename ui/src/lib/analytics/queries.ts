/**
 * Usage analytics for site admins: `site.analytics` (computed from the local event logs;
 * nothing leaves the instance), `site.usageStatistics` (DAU/WAU/MAU) and `site.users` with
 * activity. Helpers for the overview and user pages.
 */
import type { AnalyticsOverviewQuery, SiteUsersActivityQuery } from '#lib/graphql-types.ts';

export type DateRange = 'LAST_WEEK' | 'LAST_MONTH' | 'LAST_THREE_MONTHS';
export type Grouping = 'DAILY' | 'WEEKLY';

export const DATE_RANGES: { value: DateRange; label: string }[] = [
	{ value: 'LAST_WEEK', label: 'Last week' },
	{ value: 'LAST_MONTH', label: 'Last month' },
	{ value: 'LAST_THREE_MONTHS', label: 'Last 3 months' }
];

export interface StatNode {
	date: string;
	count: number;
	uniqueUsers: number;
	registeredUsers: number;
}

export interface StatSummary {
	totalCount: number;
	totalUniqueUsers: number;
	totalRegisteredUsers: number;
}

export interface StatItem {
	summary: StatSummary;
	nodes: StatNode[];
}

const STAT = `summary { totalCount totalUniqueUsers totalRegisteredUsers } nodes { date count uniqueUsers registeredUsers }`;

export const ANALYTICS_OVERVIEW = `query AnalyticsOverview($range: AnalyticsDateRange!, $grouping: AnalyticsGrouping!) {
	site {
		analytics {
			updatedAt
			users(dateRange: $range, grouping: $grouping) {
				activity { ${STAT} }
				frequencies { daysUsed frequency percentage }
				monthlyActiveUsers { date count }
				signIns { ${STAT} }
			}
			search(dateRange: $range, grouping: $grouping) {
				searches { ${STAT} }
				resultClicks { ${STAT} }
				fileViews { ${STAT} }
				searchesByType { key count }
				latency { p50 p90 p99 }
			}
			codeIntel(dateRange: $range, grouping: $grouping) {
				definitionClicks { ${STAT} }
				referenceClicks { ${STAT} }
				preciseEvents { ${STAT} }
				searchBasedEvents { ${STAT} }
				crossRepoEvents { ${STAT} }
			}
			codeIntelByLanguage(dateRange: $range) { language precision count }
			codeIntelTopRepositories(dateRange: $range) { name language kind precision events hasPrecise }
			repos { count preciseCodeIntelCount }
			notebooks(dateRange: $range, grouping: $grouping) { creations { ${STAT} } views { ${STAT} } }
			batchChanges(dateRange: $range, grouping: $grouping) {
				changesetsCreated { ${STAT} }
				changesetsMerged { ${STAT} }
				views { ${STAT} }
			}
			codeInsights(dateRange: $range, grouping: $grouping) { creations { ${STAT} } views { ${STAT} } }
			codeMonitors(dateRange: $range, grouping: $grouping) { creations { ${STAT} } views { ${STAT} } }
		}
		usageStatistics(days: 1, weeks: 1, months: 1) {
			daus { userCount }
			waus { userCount }
			maus { userCount }
		}
	}
}`;

export type AnalyticsOverview = AnalyticsOverviewQuery;

export type UserOrder = 'USERNAME' | 'EMAIL' | 'EVENTS_COUNT' | 'LAST_ACTIVE_AT' | 'CREATED_AT' | 'SITE_ADMIN';

export const SITE_USERS = `query SiteUsersActivity($query: String, $lastActiveAt: SiteUsersDateRangeInput, $limit: Int!, $offset: Int!, $orderBy: SiteUserOrderBy!, $descending: Boolean!) {
	site {
		users(query: $query, lastActiveAt: $lastActiveAt, deletedAt: { empty: true }) {
			totalCount
			nodes(limit: $limit, offset: $offset, orderBy: $orderBy, descending: $descending) {
				id
				username
				displayName
				email
				siteAdmin
				createdAt
				lastActiveAt
				eventsCount
				locked
			}
		}
	}
}`;

export interface SiteUser {
	id: string;
	username: string;
	displayName: string | null;
	email: string | null;
	siteAdmin: boolean;
	createdAt: string;
	lastActiveAt: string | null;
	eventsCount: number;
	locked: boolean;
}

export type SiteUsersResult = SiteUsersActivityQuery;

/** Activity filters of the user list. */
export type ActivityFilter = 'all' | 'active-week' | 'active-month' | 'inactive-month' | 'never';

/** The `lastActiveAt` filter of an activity filter, as of `now`. */
export function lastActiveFilter(
	f: ActivityFilter,
	now: Date = new Date()
): { gte?: string; not?: boolean; empty?: boolean } | null {
	const daysAgo = (n: number) => new Date(now.getTime() - n * 86_400_000).toISOString();
	switch (f) {
		case 'active-week':
			return { gte: daysAgo(7) };
		case 'active-month':
			return { gte: daysAgo(30) };
		case 'inactive-month':
			// Not active in the last 30 days, including users never active.
			return { gte: daysAgo(30), not: true };
		case 'never':
			return { empty: true };
		default:
			return null;
	}
}

/** A compact number: 950, 1.2k, 3.4M. */
export function formatCount(n: number): string {
	const abs = Math.abs(n);
	if (abs >= 1e6) return `${trim(n / 1e6)}M`;
	if (abs >= 1e4) return `${Math.round(n / 1e3)}k`;
	if (abs >= 1e3) return `${trim(n / 1e3)}k`;
	return String(Math.round(n));
}

function trim(x: number): string {
	return x.toFixed(1).replace(/\.0$/, '');
}

/** A latency in ms as "≤ 250 ms" / "≤ 2.5 s" (bucket upper bounds), or "–". */
export function formatLatency(ms: number | null): string {
	if (ms === null) return '–';
	return ms >= 1000 ? `≤ ${trim(ms / 1000)} s` : `≤ ${ms} ms`;
}

/** The label of a bucket start (`2026-10-19T00:00:00Z`): "Oct 19" (UTC). */
export function bucketLabel(date: string, grouping: Grouping): string {
	const d = new Date(date);
	const label = d.toLocaleDateString('en-US', { month: 'short', day: 'numeric', timeZone: 'UTC' });
	return grouping === 'WEEKLY' ? `Week of ${label}` : label;
}

/** Bar geometry of a series in a `width` × `height` box: one bar per node, scaled to the max. */
export function bars(
	values: number[],
	width: number,
	height: number,
	gap = 2
): { x: number; y: number; w: number; h: number }[] {
	if (values.length === 0) return [];
	const max = Math.max(...values, 0);
	const slot = width / values.length;
	const w = Math.max(1, slot - gap);
	return values.map((v, i) => {
		const h = max > 0 ? (Math.max(v, 0) / max) * height : 0;
		return { x: i * slot + gap / 2, y: height - h, w, h };
	});
}

/** The share of precise code navigation, in percent, or null without events. */
export function preciseShare(precise: number, searchBased: number): number | null {
	const total = precise + searchBased;
	return total > 0 ? Math.round((precise * 100) / total) : null;
}

/** "3 days ago"-style relative time for last activity, or "Never". */
export function relativeTime(iso: string | null, now: Date = new Date()): string {
	if (!iso) return 'Never';
	const s = Math.max(0, (now.getTime() - new Date(iso).getTime()) / 1000);
	if (s < 60) return 'just now';
	const units: [number, string][] = [
		[86_400 * 365, 'year'],
		[86_400 * 30, 'month'],
		[86_400, 'day'],
		[3600, 'hour'],
		[60, 'minute']
	];
	for (const [size, name] of units) {
		if (s >= size) {
			const n = Math.floor(s / size);
			return `${n} ${name}${n === 1 ? '' : 's'} ago`;
		}
	}
	return 'just now';
}
