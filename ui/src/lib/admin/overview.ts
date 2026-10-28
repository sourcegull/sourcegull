/**
 * The site admin overview page: instance health (status messages,
 * clone and index progress, repo node disks) and counts.
 * Two queries, so that a failing count does not hide the health section.
 */
import type { StatusMessage } from '#lib/status-messages.ts';
import { STATUS_MESSAGES } from '#lib/status-messages.ts';
import type { RepositoryStats } from '#lib/repos-admin/queries.ts';
import type { SiteAdminCountsQuery, SiteAdminHealthQuery } from '#lib/graphql-types.ts';

const STATUS_FIELDS = STATUS_MESSAGES.slice(
	STATUS_MESSAGES.indexOf('statusMessages'),
	STATUS_MESSAGES.lastIndexOf('}')
);

export const SITE_ADMIN_HEALTH = `query SiteAdminHealth {
	site { productVersion }
	repositoryStats {
		total cloned cloning notCloned queued failedFetch indexed notIndexed corrupted gitDirBytes
	}
	gitservers {
		totalCount
		nodes { id address freeDiskSpaceBytes totalDiskSpaceBytes error clonesPaused repositoriesCount }
	}
	${STATUS_FIELDS}
}`;

export type SiteAdminHealthResult = SiteAdminHealthQuery;

export const SITE_ADMIN_COUNTS = `query SiteAdminCounts {
	users(first: 1) { totalCount }
	organizations(first: 1) { totalCount }
	externalServices(first: 1) { totalCount }
	accessRequestsCount(status: PENDING)
}`;

export type SiteAdminCountsResult = SiteAdminCountsQuery;

/** Percent of a repo node's disk in use, or null when the size is unknown. */
export function diskUsedPercent(n: { freeDiskSpaceBytes: string; totalDiskSpaceBytes: string }): number | null {
	const total = Number(n.totalDiskSpaceBytes);
	const free = Number(n.freeDiskSpaceBytes);
	if (!Number.isFinite(total) || total <= 0 || !Number.isFinite(free)) return null;
	return Math.min(100, Math.max(0, Math.round(((total - free) * 100) / total)));
}
