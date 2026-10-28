import type { RankingSummaryResult } from '#lib/queries.ts';

type Summary = RankingSummaryResult['rankingSummary'];

function ago(iso: string, now: Date): string {
	const secs = Math.max(0, Math.round((now.getTime() - new Date(iso).getTime()) / 1000));
	if (secs < 60) return `${secs}s ago`;
	if (secs < 3600) return `${Math.round(secs / 60)}m ago`;
	if (secs < 86400) return `${Math.round(secs / 3600)}h ago`;
	return `${Math.round(secs / 86400)}d ago`;
}

function plural(n: number, one: string, many: string): string {
	return `${n.toLocaleString('en-US')} ${n === 1 ? one : many}`;
}

/** The one-line ranking status of the code graph admin page. */
export function rankingStatus(s: Summary, now: Date): { text: string; failed: boolean } {
	if (!s.enabled) {
		return {
			text: 'off (codeIntelRanking.documentReferenceCountsEnabled is false); search results are not ordered by the code graph.',
			failed: false
		};
	}
	const last = s.rankingSummary[0];
	if (!last) {
		return {
			text: 'no runs yet: files are ranked once precise indexes serve a default branch.',
			failed: false
		};
	}
	if (last.failureMessage) {
		return { text: `the last run failed ${ago(last.startedAt, now)}: ${last.failureMessage}`, failed: true };
	}
	if (!last.completedAt) {
		return { text: `computing since ${ago(last.startedAt, now)}…`, failed: false };
	}
	return {
		text:
			`${plural(s.numRankedPaths, 'file', 'files')} in ${plural(s.numRankedRepositories, 'repository', 'repositories')} ` +
			`ranked from ${plural(s.numExportedIndexes, 'index', 'indexes')}, ${ago(last.completedAt, now)}.`,
		failed: false
	};
}
