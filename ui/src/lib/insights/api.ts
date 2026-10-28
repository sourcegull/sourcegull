/**
 * Loaders shared by the trends pages.
 */
import { graphql } from '#lib/graphql.ts';
import {
	INSIGHTS_DASHBOARD,
	INSIGHTS_DASHBOARDS,
	INSIGHTS_DASHBOARD_OWNERS,
	type DashboardNode,
	type DashboardWithViews,
	type InsightsDashboardOwnersResult,
	type InsightsDashboardResult,
	type InsightsDashboardsResult,
	type OrgRef
} from './queries.ts';

export interface DashboardOwners {
	userId: string | null;
	orgs: OrgRef[];
}

export async function loadOwners(): Promise<DashboardOwners> {
	const r = await graphql<InsightsDashboardOwnersResult>(INSIGHTS_DASHBOARD_OWNERS);
	return { userId: r.currentUser?.id ?? null, orgs: r.currentUser?.organizations.nodes ?? [] };
}

export async function loadDashboards(): Promise<DashboardNode[]> {
	const r = await graphql<InsightsDashboardsResult>(INSIGHTS_DASHBOARDS);
	return r.insightsDashboards.nodes;
}

/** A dashboard with its view ids; null when it does not exist or the viewer may not see it. */
export async function loadDashboard(id: string): Promise<DashboardWithViews | null> {
	const r = await graphql<InsightsDashboardResult, { id: string }>(INSIGHTS_DASHBOARD, { id });
	return r.insightsDashboards.nodes[0] ?? null;
}

/** The view ids of a dashboard, without nulls (views deleted meanwhile). */
export function viewIdsOf(d: DashboardWithViews): string[] {
	return (d.views?.nodes ?? []).filter((n): n is { id: string } => n !== null).map((n) => n.id);
}
