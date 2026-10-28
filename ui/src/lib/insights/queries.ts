/**
 * GraphQL operations and types of the trends pages:
 * dashboards, their grants and views, and the per-view filters.
 */

import type {
	CheckRepositoryExistsQuery,
	CreateInsightsDashboardMutation,
	CreateLineChartSearchInsightMutation,
	CreatePieChartSearchInsightMutation,
	InsightPreviewQuery,
	InsightRepositoriesCountQuery,
	InsightSuggestionsQuery,
	InsightViewIdsQuery,
	InsightViewQuery,
	InsightsDashboardOwnersQuery,
	InsightsDashboardQuery,
	InsightsDashboardsQuery,
	LanguageStatisticsPreviewQuery,
	UpdateInsightsDashboardMutation,
	ValidateInsightRepoQueryQuery
} from '#lib/graphql-types.ts';

export type SeriesSortMode = 'RESULT_COUNT' | 'LEXICOGRAPHICAL' | 'DATE_ADDED';
export type SeriesSortDirection = 'ASC' | 'DESC';
export type GroupByField = 'REPO' | 'LANG' | 'PATH' | 'AUTHOR' | 'DATE';
export type TimeIntervalStepUnit = 'HOUR' | 'DAY' | 'WEEK' | 'MONTH' | 'YEAR';

export interface PermissionGrants {
	users: string[];
	organizations: string[];
	global: boolean;
}

export interface PermissionGrantsInput {
	users?: string[];
	organizations?: string[];
	global?: boolean;
}

export interface DashboardNode {
	id: string;
	title: string;
	grants: PermissionGrants;
}

export interface DashboardWithViews extends DashboardNode {
	views: { nodes: ({ id: string } | null)[]; totalCount: number | null } | null;
}

const DASHBOARD_FIELDS = `id title grants { users organizations global }`;

export const INSIGHTS_DASHBOARDS = `query InsightsDashboards {
	insightsDashboards(first: 1000) {
		nodes { ${DASHBOARD_FIELDS} }
	}
}`;

export type InsightsDashboardsResult = InsightsDashboardsQuery;

export const INSIGHTS_DASHBOARD = `query InsightsDashboard($id: ID!) {
	insightsDashboards(id: $id) {
		nodes {
			${DASHBOARD_FIELDS}
			views(first: 200) { nodes { id } totalCount }
		}
	}
}`;

export type InsightsDashboardResult = InsightsDashboardQuery;

export const CREATE_INSIGHTS_DASHBOARD = `mutation CreateInsightsDashboard($input: CreateInsightsDashboardInput!) {
	createInsightsDashboard(input: $input) { dashboard { ${DASHBOARD_FIELDS} } }
}`;

export type CreateInsightsDashboardResult = CreateInsightsDashboardMutation;

export const UPDATE_INSIGHTS_DASHBOARD = `mutation UpdateInsightsDashboard($id: ID!, $input: UpdateInsightsDashboardInput!) {
	updateInsightsDashboard(id: $id, input: $input) { dashboard { ${DASHBOARD_FIELDS} } }
}`;

export type UpdateInsightsDashboardResult = UpdateInsightsDashboardMutation;

export const DELETE_INSIGHTS_DASHBOARD = `mutation DeleteInsightsDashboard($id: ID!) {
	deleteInsightsDashboard(id: $id) { alwaysNil }
}`;

export const ADD_INSIGHT_VIEW_TO_DASHBOARD = `mutation AddInsightViewToDashboard($insightViewId: ID!, $dashboardId: ID!) {
	addInsightViewToDashboard(input: { insightViewId: $insightViewId, dashboardId: $dashboardId }) {
		dashboard { id }
	}
}`;

export const REMOVE_INSIGHT_VIEW_FROM_DASHBOARD = `mutation RemoveInsightViewFromDashboard($insightViewId: ID!, $dashboardId: ID!) {
	removeInsightViewFromDashboard(input: { insightViewId: $insightViewId, dashboardId: $dashboardId }) {
		dashboard { id }
	}
}`;

export interface DashboardViewVariables {
	insightViewId: string;
	dashboardId: string;
}

/** The signed-in user and their organizations: who a dashboard can be granted to. */
export const INSIGHTS_DASHBOARD_OWNERS = `query InsightsDashboardOwners {
	currentUser {
		id
		username
		siteAdmin
		organizations { nodes { id name displayName } }
	}
}`;

export interface OrgRef {
	id: string;
	name: string;
	displayName: string | null;
}

export type InsightsDashboardOwnersResult = InsightsDashboardOwnersQuery;

// ---- Views -------------------------------------------------------------------------------------

export interface ViewFilters {
	includeRepoRegex: string | null;
	excludeRepoRegex: string | null;
	searchContexts: string[] | null;
}

export interface SeriesDisplayOptions {
	sortOptions: { mode: SeriesSortMode | null; direction: SeriesSortDirection | null };
	limit: number | null;
	numSamples: number | null;
}

export interface FiltersInput {
	includeRepoRegex: string | null;
	excludeRepoRegex: string | null;
	searchContexts: string[];
}

export interface SeriesDisplayOptionsInput {
	sortOptions: { mode: SeriesSortMode; direction: SeriesSortDirection };
	limit: number | null;
	numSamples: number | null;
}

export type RepositoryDefinition =
	| { __typename: 'InsightRepositoryScope'; repositories: string[] }
	| { __typename: 'RepositorySearchScope'; search: string; allRepositories: boolean };

export interface SeriesDefinition {
	__typename: 'SearchInsightDataSeriesDefinition';
	seriesId: string;
	query: string;
	repositoryDefinition: RepositoryDefinition;
	timeScope: { __typename: 'InsightIntervalTimeScope'; unit: TimeIntervalStepUnit; value: number };
	generatedFromCaptureGroups: boolean;
	isCalculated: boolean;
	groupBy: GroupByField | null;
}

export type Presentation =
	| {
			__typename: 'LineChartInsightViewPresentation';
			title: string;
			seriesPresentation: { seriesId: string; label: string; color: string }[];
	  }
	| { __typename: 'PieChartInsightViewPresentation'; title: string; otherThreshold: number };

export interface DataPoint {
	dateTime: string;
	value: number;
	diffQuery: string | null;
	pointInTimeQuery: string | null;
}

export interface SeriesStatus {
	isLoadingData: boolean | null;
	pendingJobs: number;
	completedJobs: number;
	failedJobs: number;
	backfillQueuedAt: string | null;
	incompleteDatapoints: { __typename: string; time: string }[];
}

export interface DataSeries {
	seriesId: string;
	label: string;
	points: DataPoint[];
	status: SeriesStatus;
}

/** Everything a dashboard card and its chart show. */
export interface InsightViewNode {
	id: string;
	isFrozen: boolean;
	dashboardReferenceCount: number;
	seriesCount: number | null;
	presentation: Presentation;
	defaultFilters: ViewFilters;
	appliedFilters: ViewFilters;
	defaultSeriesDisplayOptions: SeriesDisplayOptions;
	appliedSeriesDisplayOptions: SeriesDisplayOptions;
	dataSeriesDefinitions: SeriesDefinition[];
	dataSeries: DataSeries[];
	languageStatistics: { name: string; totalLines: number }[];
}

const FILTER_FIELDS = `includeRepoRegex excludeRepoRegex searchContexts`;
const DISPLAY_FIELDS = `sortOptions { mode direction } limit numSamples`;

export const INSIGHT_VIEW_FIELDS = `
	id
	isFrozen
	dashboardReferenceCount
	seriesCount
	presentation {
		__typename
		... on LineChartInsightViewPresentation { title seriesPresentation { seriesId label color } }
		... on PieChartInsightViewPresentation { title otherThreshold }
	}
	defaultFilters { ${FILTER_FIELDS} }
	appliedFilters { ${FILTER_FIELDS} }
	defaultSeriesDisplayOptions { ${DISPLAY_FIELDS} }
	appliedSeriesDisplayOptions { ${DISPLAY_FIELDS} }
	dataSeriesDefinitions {
		__typename
		... on SearchInsightDataSeriesDefinition {
			seriesId
			query
			repositoryDefinition {
				__typename
				... on InsightRepositoryScope { repositories }
				... on RepositorySearchScope { search allRepositories }
			}
			timeScope { __typename ... on InsightIntervalTimeScope { unit value } }
			generatedFromCaptureGroups
			isCalculated
			groupBy
		}
	}
	dataSeries {
		seriesId
		label
		points { dateTime value diffQuery pointInTimeQuery }
		status {
			isLoadingData pendingJobs completedJobs failedJobs backfillQueuedAt
			incompleteDatapoints { __typename time }
		}
	}
	languageStatistics { name totalLines }`;

/** One view with the filters and display options a card applies (none: the saved ones). */
export const INSIGHT_VIEW = `query InsightView($id: ID!, $filters: InsightViewFiltersInput, $seriesDisplayOptions: SeriesDisplayOptionsInput) {
	insightViews(id: $id, filters: $filters, seriesDisplayOptions: $seriesDisplayOptions) {
		nodes {${INSIGHT_VIEW_FIELDS}
		}
	}
}`;

export interface InsightViewVariables {
	id: string;
	filters?: FiltersInput | null;
	seriesDisplayOptions?: SeriesDisplayOptionsInput | null;
}

export type InsightViewResult = InsightViewQuery;

/** A page of views (the "All trends" tab). */
export const INSIGHT_VIEW_IDS = `query InsightViewIds($first: Int!, $after: String) {
	insightViews(first: $first, after: $after) {
		nodes { id }
		pageInfo { hasNextPage endCursor }
		totalCount
	}
}`;

export type InsightViewIdsResult = InsightViewIdsQuery;

/** Views to pick from in the "Add or remove trends" dialog. */
export const INSIGHT_SUGGESTIONS = `query InsightSuggestions($first: Int!, $after: String, $find: String) {
	insightViews(first: $first, after: $after, find: $find) {
		nodes {
			id
			presentation {
				__typename
				... on LineChartInsightViewPresentation { title }
				... on PieChartInsightViewPresentation { title }
			}
			dataSeriesDefinitions {
				... on SearchInsightDataSeriesDefinition { query generatedFromCaptureGroups groupBy }
			}
		}
		pageInfo { hasNextPage endCursor }
		totalCount
	}
}`;

export interface InsightSuggestion {
	id: string;
	presentation: { __typename: Presentation['__typename']; title: string };
	dataSeriesDefinitions: { query: string; generatedFromCaptureGroups: boolean; groupBy: GroupByField | null }[];
}

export type InsightSuggestionsResult = InsightSuggestionsQuery;

export const DELETE_INSIGHT_VIEW = `mutation DeleteInsightView($id: ID!) {
	deleteInsightView(id: $id) { alwaysNil }
}`;

/** Saves a line chart again with new default filters and display options. */
export const UPDATE_LINE_CHART_SEARCH_INSIGHT = `mutation UpdateLineChartSearchInsight($id: ID!, $input: UpdateLineChartSearchInsightInput!) {
	updateLineChartSearchInsight(id: $id, input: $input) { view { id } }
}`;

export interface RepositoryScopeInput {
	repositories: string[];
	repositoryCriteria: string | null;
}

export interface TimeScopeInput {
	stepInterval: { unit: TimeIntervalStepUnit; value: number };
}

/** A series of a line chart; `seriesId` keeps an existing series on updates. */
export interface LineChartSeriesInput {
	seriesId?: string | null;
	query: string;
	options: { label: string | null; lineColor: string | null };
	repositoryScope?: RepositoryScopeInput;
	timeScope: TimeScopeInput;
	generatedFromCaptureGroups: boolean;
	groupBy: GroupByField | null;
}

export interface ViewControlsInput {
	filters: FiltersInput;
	seriesDisplayOptions: SeriesDisplayOptionsInput;
}

export interface UpdateLineChartSearchInsightInput {
	dataSeries: LineChartSeriesInput[];
	repositoryScope?: RepositoryScopeInput;
	presentationOptions: { title: string };
	viewControls: ViewControlsInput;
}

export const SAVE_INSIGHT_AS_NEW_VIEW = `mutation SaveInsightAsNewView($input: SaveInsightAsNewViewInput!) {
	saveInsightAsNewView(input: $input) { view { id } }
}`;

export interface SaveInsightAsNewViewInput {
	insightViewId: string;
	options: { title: string };
	dashboard: string | null;
	viewControls: { filters: FiltersInput; seriesDisplayOptions: SeriesDisplayOptionsInput };
}

// ---- Creation and edit -------------------------------------------------------------------------

export interface LineChartSearchInsightInput {
	dataSeries: LineChartSeriesInput[];
	repositoryScope: RepositoryScopeInput;
	options: { title: string };
	dashboards?: string[];
	viewControls: ViewControlsInput;
}

export interface PieChartSearchInsightInput {
	query: string;
	repositoryScope: RepositoryScopeInput;
	presentationOptions: { title: string; otherThreshold: number };
	dashboards?: string[];
}

export type UpdatePieChartSearchInsightInput = Omit<PieChartSearchInsightInput, 'dashboards'>;

export const CREATE_LINE_CHART_SEARCH_INSIGHT = `mutation CreateLineChartSearchInsight($input: LineChartSearchInsightInput!) {
	createLineChartSearchInsight(input: $input) { view { id } }
}`;

export type CreateLineChartSearchInsightResult = CreateLineChartSearchInsightMutation;

export const CREATE_PIE_CHART_SEARCH_INSIGHT = `mutation CreatePieChartSearchInsight($input: PieChartSearchInsightInput!) {
	createPieChartSearchInsight(input: $input) { view { id } }
}`;

export type CreatePieChartSearchInsightResult = CreatePieChartSearchInsightMutation;

export const UPDATE_PIE_CHART_SEARCH_INSIGHT = `mutation UpdatePieChartSearchInsight($id: ID!, $input: UpdatePieChartSearchInsightInput!) {
	updatePieChartSearchInsight(id: $id, input: $input) { view { id } }
}`;

/** Series counted on the fly for a creation page's live preview (nothing is stored). */
export const INSIGHT_PREVIEW = `query InsightPreview($input: SearchInsightPreviewInput!) {
	searchInsightPreview(input: $input) {
		label
		points { dateTime value diffQuery pointInTimeQuery }
	}
}`;

export interface InsightPreviewInput {
	repositoryScope: RepositoryScopeInput;
	timeScope: TimeScopeInput;
	series: { query: string; label: string; generatedFromCaptureGroups: boolean; groupBy: GroupByField | null }[];
}

export interface PreviewSeries {
	label: string;
	points: { dateTime: string; value: number; diffQuery: string | null; pointInTimeQuery: string | null }[];
}

export type InsightPreviewResult = InsightPreviewQuery;

/** The language usage preview: the statistics a pie over these repositories would show. */
export const LANGUAGE_STATISTICS_PREVIEW = `query LanguageStatisticsPreview($repositories: [String!]!) {
	previewLanguageStatistics(repositories: $repositories) { name totalLines }
}`;

export type LanguageStatisticsPreviewResult = LanguageStatisticsPreviewQuery;

export const VALIDATE_INSIGHT_REPO_QUERY = `query ValidateInsightRepoQuery($query: String!) {
	validateScopedInsightQuery(query: $query) { isValid invalidReason }
}`;

export type ValidateInsightRepoQueryResult = ValidateInsightRepoQueryQuery;

export const INSIGHT_REPOSITORIES_COUNT = `query InsightRepositoriesCount($query: String!) {
	previewRepositoriesFromQuery(query: $query) { numberOfRepositories }
}`;

export type InsightRepositoriesCountResult = InsightRepositoriesCountQuery;

export const CHECK_REPOSITORY_EXISTS = `query CheckRepositoryExists($name: String!) {
	repository(name: $name) { name }
}`;

export type CheckRepositoryExistsResult = CheckRepositoryExistsQuery;
