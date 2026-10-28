import type { PreciseIndexCoverageNode, PreciseIndexCoverageState } from '#lib/queries.ts';

export interface CoverageStateInfo {
	state: PreciseIndexCoverageState;
	label: string;
	badge: string;
	help: string;
}

const NOT_TARGETED: CoverageStateInfo = {
	state: 'NOT_TARGETED',
	label: 'Not targeted',
	badge: '',
	help: 'No indexing policy applies and nothing was uploaded.'
};

/** Coverage states in display order, with their labels, badge classes and explanations. */
export const COVERAGE_STATES: CoverageStateInfo[] = [
	{
		state: 'UP_TO_DATE',
		label: 'Up to date',
		badge: 'badge-success',
		help: 'A completed index serves the default branch.'
	},
	{
		state: 'STALE',
		label: 'Stale',
		badge: 'badge-warning',
		help: 'The newest index is not at the default branch tip and is older than the stale window.'
	},
	{
		state: 'FAILING',
		label: 'Failing',
		badge: 'badge-danger',
		help: 'The latest index job or upload failed.'
	},
	{
		state: 'NONE',
		label: 'Not indexed',
		badge: 'badge-warning',
		help: 'An indexing policy applies, but nothing is indexed yet.'
	},
	NOT_TARGETED
];

export function stateInfo(state: PreciseIndexCoverageState): CoverageStateInfo {
	return COVERAGE_STATES.find((s) => s.state === state) ?? NOT_TARGETED;
}

/** `3h ago`, `12d ago`. */
export function ago(iso: string, now: Date): string {
	const secs = Math.max(0, Math.round((now.getTime() - new Date(iso).getTime()) / 1000));
	if (secs < 60) return `${secs}s ago`;
	if (secs < 3600) return `${Math.round(secs / 60)}m ago`;
	if (secs < 86400) return `${Math.round(secs / 3600)}h ago`;
	return `${Math.round(secs / 86400)}d ago`;
}

/** The indexed commit cell: the short commit, whether it is the default branch tip, and its age. */
export function indexedSummary(n: PreciseIndexCoverageNode, now: Date): string {
	if (!n.indexedCommit) return 'never';
	const parts = [n.indexedCommit.slice(0, 12)];
	if (n.indexedAtHead) parts.push('default branch tip');
	else if (n.headCommit) parts.push(`tip is ${n.headCommit.slice(0, 12)}`);
	if (n.indexedAt) parts.push(ago(n.indexedAt, now));
	return parts.join(' · ');
}
