/**
 * Result and variable types of the UI's GraphQL operations, generated from ui/schema.graphql by
 * src/lib/graphql-types.test.ts. Do not edit; run `npm run graphql-types`.
 */
/* eslint-disable */
/** Internal type. DO NOT USE DIRECTLY. */
type Exact<T extends { [key: string]: unknown }> = { [K in keyof T]: T[K] };
/** Internal type. DO NOT USE DIRECTLY. */
export type Incremental<T> = T | { [P in keyof T]?: P extends ' $fragmentName' | '__typename' ? T[P] : never };
/** The state of an access request. */
export type AccessRequestStatus =
  | 'PENDING'
  | 'APPROVED'
  | 'REJECTED';

/** Input of `addExternalService`. */
export type AddExternalServiceInput = {
  /** The code host kind. */
  kind: ExternalServiceKind;
  /** The name shown in the UI. */
  displayName: string;
  /** The JSONC configuration. */
  config: string;
};

/** A predefined range of days, ending today. */
export type AnalyticsDateRange =
  | 'LAST_THREE_MONTHS'
  | 'LAST_MONTH'
  | 'LAST_WEEK'
  /** Not supported: fails with `INVALID_INPUT`. */
  | 'CUSTOM';

/** How a time series is bucketed. */
export type AnalyticsGrouping =
  | 'DAILY'
  | 'WEEKLY';

/** Who to assign to what. */
export type AssignOwnerOrTeamInput = {
  /** A `User` ID (`assignOwner`) or a `Team` ID (`assignTeam`). */
  assignedOwnerID: string | number;
  repoID: string | number;
  /** A file or directory path, or `""` for the whole repository. */
  absolutePath: string;
};

/** What kind of routine it is. */
export type BackgroundRoutineType =
  /** Periodic routine. */
  | 'PERIODIC'
  /** Periodic routine with metrics set up. */
  | 'PERIODIC_WITH_METRICS'
  /** DB-backed worker. */
  | 'DB_BACKED'
  /** Custom routine. */
  | 'CUSTOM';

/** The state of a bulk change. */
export type BatchChangeState =
  | 'OPEN'
  | 'CLOSED'
  | 'DRAFT';

/** Where a bulk change spec was executed. */
export type BatchSpecSource =
  | 'LOCAL'
  | 'REMOTE';

/** The state of a bulk change spec. */
export type BatchSpecState =
  | 'PENDING'
  | 'PROCESSING'
  | 'FAILED'
  | 'COMPLETED'
  | 'QUEUED'
  | 'CANCELING'
  | 'CANCELED';

/** The state of a workspace resolution. */
export type BatchSpecWorkspaceResolutionState =
  | 'QUEUED'
  | 'PROCESSING'
  | 'ERRORED'
  | 'FAILED'
  | 'COMPLETED';

/** The state of a workspace's execution. */
export type BatchSpecWorkspaceState =
  | 'SKIPPED'
  | 'PENDING'
  | 'QUEUED'
  | 'PROCESSING'
  | 'FAILED'
  | 'COMPLETED'
  | 'CANCELING'
  | 'CANCELED';

/** The state of a bulk operation. */
export type BulkOperationState =
  | 'PROCESSING'
  | 'FAILED'
  | 'COMPLETED';

/** The kind of a bulk operation. */
export type BulkOperationType =
  | 'COMMENT'
  | 'DETACH'
  | 'REENQUEUE'
  | 'MERGE'
  | 'CLOSE'
  | 'PUBLISH';

/** The CI state of a changeset (`UNKNOWN` is shown as null). */
export type ChangesetCheckState =
  | 'PENDING'
  | 'PASSED'
  | 'FAILED';

/** The review state of a changeset. */
export type ChangesetReviewState =
  | 'APPROVED'
  | 'CHANGES_REQUESTED'
  | 'PENDING'
  | 'COMMENTED'
  | 'DISMISSED';

/** An operation the reconciler carries out on a changeset. */
export type ChangesetSpecOperation =
  | 'PUSH'
  | 'UPDATE'
  | 'UNDRAFT'
  | 'PUBLISH'
  | 'PUBLISH_DRAFT'
  | 'SYNC'
  | 'IMPORT'
  | 'CLOSE'
  | 'REOPEN'
  | 'SLEEP'
  | 'DETACH'
  | 'ARCHIVE'
  | 'REATTACH';

/** The publication of one changeset spec chosen in the UI. */
export type ChangesetSpecPublicationStateInput = {
  /** The changeset spec (whose `published` must be unset). */
  changesetSpec: string | number;
  /** `true`, `false` or `"draft"`. */
  publicationState: boolean | 'draft';
};

/** What a changeset spec describes. */
export type ChangesetSpecType =
  | 'EXISTING'
  | 'BRANCH';

/** The state of a changeset shown in the UI. */
export type ChangesetState =
  | 'UNPUBLISHED'
  | 'SCHEDULED'
  | 'PROCESSING'
  | 'RETRYING'
  | 'FAILED'
  | 'OPEN'
  | 'DRAFT'
  | 'CLOSED'
  | 'MERGED'
  | 'READONLY'
  | 'DELETED';

/** Filter on clone status. */
export type CloneStatus =
  | 'NOT_CLONED'
  | 'CLONING'
  | 'CLONED';

/** Where code graph data comes from. */
export type CodeGraphDataProvenance =
  /** A compiler, type checker or similar source without false positives. */
  | 'PRECISE'
  /** Syntax trees without type information. */
  | 'SYNTACTIC'
  /** Text search. */
  | 'SEARCH_BASED';

/** Why a user has to sign in with a code host again. */
export type CodeHostSignInReason =
  /** The code host rejected the stored token (revoked, or its refresh failed). */
  | 'EXPIRED'
  /** The token lacks the scope the permissions sync needs to list private repositories. */
  | 'MISSING_SCOPE';

/**
 * The input of `addCodeownersFile` and `updateCodeownersFile`: the text and the repository
 * (by ID or by name).
 */
export type CodeownersFileInput = {
  fileContents: string;
  repoID?: string | number | null | undefined;
  repoName?: string | null | undefined;
};

/** CreateFileBlockInput contains the information necessary to create a file block. */
export type CreateFileBlockInput = {
  /** Name of the repository, e.g. "gitlab.com/group/project". */
  repositoryName: string;
  /** Path within the repository. */
  filePath: string;
  /** An optional revision; if omitted, the default branch. */
  revision?: string | null | undefined;
  /** An optional line range; if omitted, the entire file. */
  lineRange?: CreateFileBlockLineRangeInput | null | undefined;
};

/** Input to create a line range for a file block. */
export type CreateFileBlockLineRangeInput = {
  /** The first line to fetch (0-indexed, inclusive). */
  startLine: number;
  /** The last line to fetch (0-indexed, exclusive). */
  endLine: number;
};

export type CreateInsightsDashboardInput = {
  title: string;
  grants: InsightsPermissionGrantsInput;
};

/**
 * GraphQL does not accept union types as inputs, so all block inputs are optional and
 * `type` selects the one used.
 */
export type CreateNotebookBlockInput = {
  /** ID of the block. */
  id: string;
  /** Block type. */
  type: NotebookBlockType;
  /** Markdown input. */
  markdownInput?: string | null | undefined;
  /** Query input. */
  queryInput?: string | null | undefined;
  /** File input. */
  fileInput?: CreateFileBlockInput | null | undefined;
  /** Symbol input. */
  symbolInput?: CreateSymbolBlockInput | null | undefined;
};

/** CreateSymbolBlockInput contains the information necessary to create a symbol block. */
export type CreateSymbolBlockInput = {
  /** Name of the repository, e.g. "gitlab.com/group/project". */
  repositoryName: string;
  /** Path within the repository. */
  filePath: string;
  /** An optional revision; if omitted, the default branch. */
  revision?: string | null | undefined;
  /** Number of lines to show before and after the matched symbol line. */
  lineContext: number;
  /** The symbol name. */
  symbolName: string;
  /** Name of the symbol container. */
  symbolContainerName: string;
  /** The symbol kind. */
  symbolKind: SymbolKind;
};

/** A repository whose ingested file to delete (by ID or by name). */
export type DeleteCodeownersFilesInput = {
  repoID?: string | number | null | undefined;
  repoName?: string | null | undefined;
};

/** The channel a watch action posts through. */
export type DeliveryChannel =
  | 'WEBHOOK'
  | 'SLACK_WEBHOOK'
  | 'MATTERMOST_WEBHOOK'
  | 'TELEGRAM';

/** The status of a trigger or action event. */
export type EventStatus =
  | 'PENDING'
  | 'SUCCESS'
  | 'ERROR';

/** How an executor's version compares with the server's. */
export type ExecutorCompatibility =
  | 'UP_TO_DATE'
  | 'OUTDATED'
  | 'TOO_NEW';

/** `ExecutorSecretScope`. */
export type ExecutorSecretScope =
  /** Batch Changes executions. */
  | 'BATCHES'
  /** Auto-indexing. */
  | 'CODEINTEL';

/** The kind of a code host connection. */
export type ExternalServiceKind =
  /** gitlab.com or a self-managed GitLab. */
  | 'GITLAB'
  /** github.com or GitHub Enterprise Server. */
  | 'GITHUB'
  /** A Gitea instance. */
  | 'GITEA';

/** The state of a sync run. */
export type ExternalServiceSyncJobState =
  | 'QUEUED'
  | 'PROCESSING'
  | 'ERRORED'
  | 'FAILED'
  | 'COMPLETED'
  /** Cancellation was requested; the worker stops at its next heartbeat. */
  | 'CANCELING'
  | 'CANCELED';

/** `GitObjectType`. Policies name commits, branches (`GIT_TREE`) or tags. */
export type GitObjectType =
  | 'GIT_COMMIT'
  | 'GIT_TAG'
  | 'GIT_TREE'
  | 'GIT_BLOB'
  | 'GIT_UNKNOWN';

/** Sort order of refs. */
export type GitRefOrder =
  /** Most recently committed first. */
  | 'AUTHORED_OR_COMMITTED_AT'
  /** By name. */
  | 'ALPHABETICAL';

/** The kind of a ref. */
export type GitRefType =
  | 'GIT_BRANCH'
  | 'GIT_TAG'
  | 'GIT_REF_OTHER';

/**
 * What a grouped series groups by (`AUTHOR` and `DATE`, the commit's date, for commit and
 * diff searches only).
 */
export type GroupByField =
  | 'REPO'
  | 'LANG'
  | 'PATH'
  | 'AUTHOR'
  | 'DATE';

/** A view's default filters and display options. */
export type InsightViewControlsInput = {
  filters: InsightViewFiltersInput;
  seriesDisplayOptions: SeriesDisplayOptionsInput;
};

/** Filters applied to a view's points. */
export type InsightViewFiltersInput = {
  includeRepoRegex?: string | null | undefined;
  excludeRepoRegex?: string | null | undefined;
  searchContexts?: Array<string> | null | undefined;
};

/**
 * Grants of a dashboard. Users and organizations must be the viewer and the viewer's
 * organizations.
 */
export type InsightsPermissionGrantsInput = {
  users?: Array<string | number> | null | undefined;
  organizations?: Array<string | number> | null | undefined;
  global?: boolean | null | undefined;
};

/** Options for a line chart's series. */
export type LineChartDataSeriesOptionsInput = {
  label?: string | null | undefined;
  lineColor?: string | null | undefined;
};

/** Options for a line chart. */
export type LineChartOptionsInput = {
  title?: string | null | undefined;
};

/** A data series of a line chart. */
export type LineChartSearchInsightDataSeriesInput = {
  /** The series to keep (updates); omit for a new series. */
  seriesId?: string | null | undefined;
  query: string;
  options: LineChartDataSeriesOptionsInput;
  /** Overrides the trend's repository scope. */
  repositoryScope?: RepositoryScopeInput | null | undefined;
  /** Overrides the trend's time scope. */
  timeScope?: TimeScopeInput | null | undefined;
  /** One series per string captured by the query's first capture group. */
  generatedFromCaptureGroups?: boolean | null | undefined;
  /** One series per repository, language, path or author (needs a repository list). */
  groupBy?: GroupByField | null | undefined;
};

export type LineChartSearchInsightInput = {
  dataSeries: Array<LineChartSearchInsightDataSeriesInput>;
  /** Applies to every series that has none. */
  repositoryScope?: RepositoryScopeInput | null | undefined;
  /** Applies to every series that has none. */
  timeScope?: TimeScopeInput | null | undefined;
  options: LineChartOptionsInput;
  /** Dashboards to add the trend to. */
  dashboards?: Array<string | number> | null | undefined;
  viewControls?: InsightViewControlsInput | null | undefined;
};

/** The input required to create an action: exactly one field set. */
export type MonitorActionInput = {
  email?: MonitorEmailInput | null | undefined;
  webhook?: MonitorWebhookInput | null | undefined;
  slackWebhook?: MonitorSlackWebhookInput | null | undefined;
  mattermostWebhook?: MonitorMattermostWebhookInput | null | undefined;
  telegram?: MonitorTelegramInput | null | undefined;
};

/** The input required to edit an action: exactly one field set. */
export type MonitorEditActionInput = {
  email?: MonitorEditEmailInput | null | undefined;
  webhook?: MonitorEditWebhookInput | null | undefined;
  slackWebhook?: MonitorEditSlackWebhookInput | null | undefined;
  mattermostWebhook?: MonitorEditMattermostWebhookInput | null | undefined;
  telegram?: MonitorEditTelegramInput | null | undefined;
};

/** The input required to edit an email action (no `id` creates one). */
export type MonitorEditEmailInput = {
  id?: string | number | null | undefined;
  update: MonitorEmailInput;
};

/** The input required to edit a watch. */
export type MonitorEditInput = {
  id: string | number;
  update: MonitorInput;
};

export type MonitorEditMattermostWebhookInput = {
  id?: string | number | null | undefined;
  update: MonitorMattermostWebhookInput;
};

export type MonitorEditSlackWebhookInput = {
  id?: string | number | null | undefined;
  update: MonitorSlackWebhookInput;
};

export type MonitorEditTelegramInput = {
  id?: string | number | null | undefined;
  update: MonitorTelegramInput;
};

/** The input required to edit a trigger. */
export type MonitorEditTriggerInput = {
  id: string | number;
  update: MonitorTriggerInput;
};

export type MonitorEditWebhookInput = {
  id?: string | number | null | undefined;
  update: MonitorWebhookInput;
};

/** The input required to create an email action. */
export type MonitorEmailInput = {
  enabled: boolean;
  includeResults: boolean;
  priority: MonitorEmailPriority;
  /** Users or organizations (every member) that receive the email. */
  recipients: Array<string | number>;
  /**
   * Use header to automatically approve the message in a read-only or moderated mailing
   * list: empty, or one `Name: value` header field (e.g. `Approved: password`).
   * On an update of a stored action, empty or the masked header keeps the stored one.
   */
  header: string;
  /** On an update of a stored action, removes its header (`header` is ignored). */
  removeHeader?: boolean;
};

/** `MonitorEmail.priority`. */
export type MonitorEmailPriority =
  | 'NORMAL'
  | 'CRITICAL';

/** The input required to create a watch. */
export type MonitorInput = {
  /** The owner: a user or an organization. */
  namespace: string | number;
  description: string;
  enabled: boolean;
  /**
   * Send every email recipient all results, found with your access, including results in
   * repositories they cannot read. Site admins only, unless
   * `watches.allowUnfilteredResults` is on.
   */
  unfilteredResults?: boolean;
};

/** The input required to create a Mattermost webhook action. */
export type MonitorMattermostWebhookInput = {
  enabled: boolean;
  includeResults: boolean;
  /** On an update of a stored action, empty or the masked URL keeps the stored one. */
  url: string;
};

/** The input required to create a Slack webhook action. */
export type MonitorSlackWebhookInput = {
  enabled: boolean;
  includeResults: boolean;
  /** On an update of a stored action, empty or the masked URL keeps the stored one. */
  url: string;
};

/** The input required to create a Telegram action. */
export type MonitorTelegramInput = {
  enabled: boolean;
  includeResults: boolean;
  /**
   * The bot's token, from BotFather (`123456:ABC-...`).
   * On an update of a stored action, empty or the masked token keeps the stored one.
   */
  botToken: string;
  /**
   * The chat the bot posts to: a numeric chat ID (negative for groups and channels) or a
   * public channel's `@username`. The bot must be a member of the chat.
   */
  chatId: string;
};

/** The input required to create a trigger. */
export type MonitorTriggerInput = {
  query: string;
};

/** The input required to create a webhook action. */
export type MonitorWebhookInput = {
  enabled: boolean;
  includeResults: boolean;
  /** On an update of a stored action, empty or the masked URL keeps the stored one. */
  url: string;
};

export type NotAvailableReasonType =
  | 'INVALID_QUERY'
  | 'INVALID_AGGREGATION_MODE_FOR_QUERY'
  | 'TIMEOUT_EXTENSION_AVAILABLE'
  | 'TIMEOUT_NO_EXTENSION_AVAILABLE'
  | 'ERROR_OCCURRED';

/** Enum of possible block types. */
export type NotebookBlockType =
  | 'MARKDOWN'
  | 'QUERY'
  | 'FILE'
  | 'SYMBOL';

/** Input for a new notebook. */
export type NotebookInput = {
  /** The title of the notebook. */
  title: string;
  /** Array of notebook blocks. */
  blocks: Array<CreateNotebookBlockInput>;
  /**
   * Notebook namespace (user or org). Controls the visibility of the notebook and who can
   * edit the notebook.
   */
  namespace: string | number;
  /** Whether every user of the instance may see the notebook. */
  public: boolean;
};

/** NotebooksOrderBy enumerates the ways notebooks can be ordered. */
export type NotebooksOrderBy =
  | 'NOTEBOOK_UPDATED_AT'
  | 'NOTEBOOK_CREATED_AT'
  | 'NOTEBOOK_STAR_COUNT';

/** A recipient's answer to an invitation. */
export type OrganizationInvitationResponseType =
  /** Accepted: the recipient joined. */
  | 'ACCEPT'
  /** Declined. */
  | 'REJECT';

/** A new webhook. */
export type OutboundWebhookCreateInput = {
  /** The `http` or `https` URL events are posted to. */
  url: string;
  /** The key of the `X-Sourcegull-Webhook-Signature` HMAC-SHA256 of each body. */
  secret: string;
  /** At least one. */
  eventTypes: Array<OutboundWebhookScopedEventTypeInput>;
};

/** An event type to subscribe to. */
export type OutboundWebhookScopedEventTypeInput = {
  /** One of `outboundWebhookEventTypes`. */
  eventType: string;
  /** Only events of this scope; null or absent for all. */
  scope?: string | null | undefined;
};

/** Changes to a webhook: the URL and event types are replaced, the secret when given. */
export type OutboundWebhookUpdateInput = {
  /** Empty or the masked URL keeps the stored one. */
  url: string;
  eventTypes: Array<OutboundWebhookScopedEventTypeInput>;
  /** A new secret (unchanged when absent). */
  secret?: string | null | undefined;
};

/** A change to one signal configuration. */
export type OwnSignalConfigurationUpdate = {
  name: string;
  /** Unset keeps the current patterns. */
  excludedRepoPatterns?: Array<string> | null | undefined;
  enabled: boolean;
};

/** The state of a permissions sync job. */
export type PermissionsSyncJobState =
  | 'QUEUED'
  | 'PROCESSING'
  | 'ERRORED'
  | 'FAILED'
  | 'COMPLETED'
  | 'CANCELED';

/** Options for a pie chart. */
export type PieChartOptionsInput = {
  title: string;
  /** Languages with a smaller share of the lines are drawn as "Other". */
  otherThreshold: number;
};

export type PieChartSearchInsightInput = {
  query: string;
  repositoryScope: RepositoryScopeInput;
  presentationOptions: PieChartOptionsInput;
  dashboards?: Array<string | number> | null | undefined;
};

/** A position (input). */
export type PositionInput = {
  line: number;
  character: number;
};

/** A repository's precise index coverage. */
export type PreciseIndexCoverageState =
  /**
   * A completed index is at the default branch tip, or the newest one is within the stale
   * window.
   */
  | 'UP_TO_DATE'
  /**
   * Completed indexes exist, none at the default branch tip, the newest older than the
   * stale window.
   */
  | 'STALE'
  /** The latest index job or upload failed after the newest completed index. */
  | 'FAILING'
  /** No completed index, though an indexing policy applies. */
  | 'NONE'
  /** No completed index and no indexing policy applies. */
  | 'NOT_TARGETED';

/** `PreciseIndex.state`. The indexing states are those of auto-indexing jobs. */
export type PreciseIndexState =
  | 'UPLOADING_INDEX'
  | 'QUEUED_FOR_PROCESSING'
  | 'PROCESSING'
  | 'PROCESSING_ERRORED'
  | 'COMPLETED'
  | 'DELETING'
  | 'DELETED'
  | 'QUEUED_FOR_INDEXING'
  | 'INDEXING'
  | 'INDEXING_ERRORED'
  | 'INDEXING_COMPLETED';

/** A source range of a usage of the symbol to look up. */
export type RangeInput = {
  repository: string;
  /** Defaults to the default branch. */
  revision?: string | null | undefined;
  path: string;
  start: PositionInput;
  end: PositionInput;
};

/** Why a code host project has no repository the repo node can fetch. */
export type RemoteUnavailableReason =
  /** No repository yet (never pushed to). */
  | 'MISSING'
  /** The project's repository feature is disabled on the code host. */
  | 'DISABLED'
  /** The code host token may not download the project's code. */
  | 'NO_CODE_ACCESS';

/** Why a user can read a repository. */
export type RepositoryAccessReasonKind =
  /** Site admins read everything (`authz.enforceForSiteAdmins` is off). */
  | 'SITE_ADMIN'
  /** The repository is public on the code host. */
  | 'PUBLIC'
  /** Marked unrestricted with `setRepositoryPermissionsUnrestricted`. */
  | 'UNRESTRICTED'
  /** Listed by a code host connection without an `authorization` block. */
  | 'UNRESTRICTED_CONNECTION'
  /** Granted with `setRepositoryPermissionsForUsers`. */
  | 'EXPLICIT_API'
  /** Granted by the code host permissions sync. */
  | 'PERMISSIONS_SYNC';

export type RepositoryFilter = {
  name: StringComparator;
};

/**
 * The repositories of a series: a list, or a query of repository filters (re-evaluated at
 * every recording); neither is every repository.
 */
export type RepositoryScopeInput = {
  repositories: Array<string>;
  repositoryCriteria?: string | null | undefined;
};

export type SaveInsightAsNewViewInput = {
  insightViewId: string | number;
  options: LineChartOptionsInput;
  dashboard?: string | number | null | undefined;
  viewControls?: InsightViewControlsInput | null | undefined;
};

/** A new saved search. */
export type SavedSearchInput = {
  /** The owning user or organization. */
  owner: string | number;
  description: string;
  query: string;
  draft: boolean;
  visibility: SavedSearchVisibility;
};

/** The editable fields of a saved search. */
export type SavedSearchUpdateInput = {
  description: string;
  query: string;
  draft: boolean;
};

/** `SavedSearch.visibility`. */
export type SavedSearchVisibility =
  /** Visible to every user. */
  | 'PUBLIC'
  /** Visible only to the owner user, or the owner org's members. */
  | 'SECRET';

/** `savedSearches(orderBy:)`. */
export type SavedSearchesOrderBy =
  | 'SAVED_SEARCH_DESCRIPTION'
  | 'SAVED_SEARCH_UPDATED_AT';

export type SearchAggregationMode =
  | 'REPO'
  | 'PATH'
  | 'AUTHOR'
  | 'CAPTURE_GROUP';

/** The name, description, visibility and query of an existing context. */
export type SearchContextEditInput = {
  name: string;
  description: string;
  public: boolean;
  query: string;
};

/** The name, description, visibility and query of a new context. */
export type SearchContextInput = {
  /** The name, without the owner (`ctx` in `@alice/ctx`). */
  name: string;
  description: string;
  public: boolean;
  /** The owning user or organization; null for an instance-level context. */
  namespace?: string | number | null | undefined;
  /** Repository filters for a query-defined context; empty for a listed one. */
  query: string;
};

/** A repository and the revisions to search in it. */
export type SearchContextRepositoryRevisionsInput = {
  repositoryID: string | number;
  /** Branches, tags or commits; empty or `HEAD` is the default branch. */
  revisions: Array<string>;
};

/** Series to preview over one repository and time scope. */
export type SearchInsightPreviewInput = {
  repositoryScope: RepositoryScopeInput;
  timeScope: TimeScopeInput;
  series: Array<SearchSeriesPreviewInput>;
};

/** `SearchJob.state`. */
export type SearchJobState =
  /** Created, waiting to be processed. */
  | 'QUEUED'
  /** Being processed. */
  | 'PROCESSING'
  /** Had an error (retried). */
  | 'ERRORED'
  /** Failed. */
  | 'FAILED'
  /** Completed. */
  | 'COMPLETED'
  /** Canceled. */
  | 'CANCELED';

/** Sort order of `searchJobs`. */
export type SearchJobsOrderBy =
  /** By query. */
  | 'QUERY'
  /** By creation date. */
  | 'CREATED_AT'
  /** By state. */
  | 'STATE';

/** The pattern types (`SearchPatternType`). */
export type SearchPatternType =
  | 'standard'
  | 'literal'
  | 'regexp'
  | 'structural'
  | 'keyword';

export type SearchSeriesPreviewInput = {
  query: string;
  label: string;
  generatedFromCaptureGroups: boolean;
  groupBy?: GroupByField | null | undefined;
};

/** Which series a view shows. */
export type SeriesDisplayOptionsInput = {
  sortOptions?: SeriesSortOptionsInput | null | undefined;
  /** Max number of series. */
  limit?: number | null | undefined;
  /** Max number of points per series (at most 90). */
  numSamples?: number | null | undefined;
};

export type SeriesSortDirection =
  | 'ASC'
  | 'DESC';

/** How series are sorted. */
export type SeriesSortMode =
  | 'RESULT_COUNT'
  | 'LEXICOGRAPHICAL'
  | 'DATE_ADDED';

export type SeriesSortOptionsInput = {
  mode: SeriesSortMode;
  direction: SeriesSortDirection;
};

/** The order of `site.users`. */
export type SiteUserOrderBy =
  | 'USERNAME'
  | 'EMAIL'
  | 'EVENTS_COUNT'
  | 'LAST_ACTIVE_AT'
  | 'CREATED_AT'
  | 'DELETED_AT'
  | 'SITE_ADMIN';

/** A timestamp filter of `site.users`. */
export type SiteUsersDateRangeInput = {
  /** Less than or equal to. */
  lte?: string | null | undefined;
  /** Greater than or equal to. */
  gte?: string | null | undefined;
  /** Negates the filter. */
  not?: boolean | null | undefined;
  /** Only null (`true`) or only set (`false`) values. */
  empty?: boolean | null | undefined;
};

/** What kind of request was slow. */
export type SlowRequestKind =
  /** A GraphQL request. */
  | 'GRAPHQL'
  /** A search. */
  | 'SEARCH';

export type StringComparator = {
  equals?: string | null | undefined;
};

/** The symbol kinds (LSP's, as `SymbolKind`). */
export type SymbolKind =
  | 'UNKNOWN'
  | 'FILE'
  | 'MODULE'
  | 'NAMESPACE'
  | 'PACKAGE'
  | 'CLASS'
  | 'METHOD'
  | 'PROPERTY'
  | 'FIELD'
  | 'CONSTRUCTOR'
  | 'ENUM'
  | 'INTERFACE'
  | 'FUNCTION'
  | 'VARIABLE'
  | 'CONSTANT'
  | 'STRING'
  | 'NUMBER'
  | 'BOOLEAN'
  | 'ARRAY'
  | 'OBJECT'
  | 'KEY'
  | 'NULL'
  | 'ENUMMEMBER'
  | 'STRUCT'
  | 'EVENT'
  | 'OPERATOR'
  | 'TYPEPARAMETER';

/** How a usage relates to the symbol. */
export type SymbolUsageKind =
  | 'DEFINITION'
  | 'REFERENCE'
  | 'IMPLEMENTATION'
  | 'SUPER';

/**
 * Who to add to or remove from a team; the first set field that matches wins (user ID,
 * username, verified email, code host account).
 */
export type TeamMemberInput = {
  userID?: string | number | null | undefined;
  username?: string | null | undefined;
  email?: string | null | undefined;
  externalAccountServiceID?: string | null | undefined;
  externalAccountServiceType?: string | null | undefined;
  externalAccountAccountID?: string | null | undefined;
  externalAccountLogin?: string | null | undefined;
};

/** A sample interval: every `value` `unit`s. */
export type TimeIntervalStepInput = {
  unit: TimeIntervalStepUnit;
  value: number;
};

/** The unit of a sample interval. */
export type TimeIntervalStepUnit =
  | 'HOUR'
  | 'DAY'
  | 'WEEK'
  | 'MONTH'
  | 'YEAR';

/** When a series records points. */
export type TimeScopeInput = {
  stepInterval?: TimeIntervalStepInput | null | undefined;
};

/** What a tree entry is. */
export type TreeEntryKind =
  | 'FILE'
  | 'EXECUTABLE'
  | 'SYMLINK'
  | 'DIRECTORY'
  | 'SUBMODULE';

/** Input of `updateExternalService`. */
export type UpdateExternalServiceInput = {
  /** The connection to update. */
  id: string | number;
  /** A new display name. */
  displayName?: string | null | undefined;
  /** A new configuration; `REDACTED` placeholders keep the stored secrets. */
  config?: string | null | undefined;
};

export type UpdateInsightsDashboardInput = {
  title?: string | null | undefined;
  grants?: InsightsPermissionGrantsInput | null | undefined;
};

export type UpdateLineChartSearchInsightInput = {
  /** Every series of the chart; a series left out is removed. */
  dataSeries: Array<LineChartSearchInsightDataSeriesInput>;
  repositoryScope?: RepositoryScopeInput | null | undefined;
  timeScope?: TimeScopeInput | null | undefined;
  presentationOptions: LineChartOptionsInput;
  viewControls: InsightViewControlsInput;
};

export type UpdatePieChartSearchInsightInput = {
  query: string;
  repositoryScope: RepositoryScopeInput;
  presentationOptions: PieChartOptionsInput;
};

/** The input of `updateOwnSignalConfigurations`. */
export type UpdateSignalConfigurationsInput = {
  configs: Array<OwnSignalConfigurationUpdate>;
};

/** Limits the usages returned. */
export type UsagesFilter = {
  not?: UsagesFilter | null | undefined;
  repository?: RepositoryFilter | null | undefined;
};

export type CodeHostSignInsNeededQueryVariables = Exact<{ [key: string]: never; }>;


export type CodeHostSignInsNeededQuery = { currentUser: { codeHostSignInsNeeded: Array<{ serviceType: string, serviceID: string, displayName: string, reason: CodeHostSignInReason, authenticationURL: string | null }> } | null };

export type AccountEmailsQueryVariables = Exact<{ [key: string]: never; }>;


export type AccountEmailsQuery = { currentUser: { id: string, scimControlled: boolean, emails: Array<{ email: string, verified: boolean, isPrimary: boolean }> } | null };

export type AccountPasswordQueryVariables = Exact<{ [key: string]: never; }>;


export type AccountPasswordQuery = { currentUser: { id: string, hasPassword: boolean } | null };

export type AccountProfileQueryVariables = Exact<{ [key: string]: never; }>;


export type AccountProfileQuery = { currentUser: { id: string, username: string, displayName: string | null, avatarURL: string | null, viewerCanChangeUsername: boolean, scimControlled: boolean } | null };

export type AddUserEmailMutationVariables = Exact<{
  user: string | number;
  email: string;
}>;


export type AddUserEmailMutation = { addUserEmail: { alwaysNil: string | null } };

export type DeactivateUserMutationVariables = Exact<{
  user: string | number;
}>;


export type DeactivateUserMutation = { deactivateUser: { alwaysNil: string | null } };

export type RandomizeUserPasswordMutationVariables = Exact<{
  user: string | number;
}>;


export type RandomizeUserPasswordMutation = { randomizeUserPassword: { resetPasswordURL: string } };

export type ReactivateUserMutationVariables = Exact<{
  user: string | number;
}>;


export type ReactivateUserMutation = { reactivateUser: { id: string } };

export type RemoveUserEmailMutationVariables = Exact<{
  user: string | number;
  email: string;
}>;


export type RemoveUserEmailMutation = { removeUserEmail: { alwaysNil: string | null } };

export type ResendVerificationEmailMutationVariables = Exact<{
  user: string | number;
  email: string;
}>;


export type ResendVerificationEmailMutation = { resendVerificationEmail: { alwaysNil: string | null } };

export type SetUserEmailPrimaryMutationVariables = Exact<{
  user: string | number;
  email: string;
}>;


export type SetUserEmailPrimaryMutation = { setUserEmailPrimary: { alwaysNil: string | null } };

export type UpdatePasswordMutationVariables = Exact<{
  oldPassword?: string | null | undefined;
  newPassword: string;
}>;


export type UpdatePasswordMutation = { updatePassword: { alwaysNil: string | null } };

export type UpdateProfileMutationVariables = Exact<{
  user: string | number;
  username?: string | null | undefined;
  displayName?: string | null | undefined;
  avatarURL?: string | null | undefined;
}>;


export type UpdateProfileMutation = { updateUser: { id: string, username: string, displayName: string | null, avatarURL: string | null } };

export type CreateFeatureFlagMutationVariables = Exact<{
  name: string;
  value?: boolean | null | undefined;
  rolloutBasisPoints?: number | null | undefined;
}>;


export type CreateFeatureFlagMutation = { createFeatureFlag:
    | { __typename: 'FeatureFlagBoolean' }
    | { __typename: 'FeatureFlagRollout' }
   };

export type CreateFeatureFlagOverrideMutationVariables = Exact<{
  namespace: string | number;
  flagName: string;
  value: boolean;
}>;


export type CreateFeatureFlagOverrideMutation = { createFeatureFlagOverride: { id: string } };

export type DeleteFeatureFlagMutationVariables = Exact<{
  name: string;
}>;


export type DeleteFeatureFlagMutation = { deleteFeatureFlag: { alwaysNil: string | null } };

export type DeleteFeatureFlagOverrideMutationVariables = Exact<{
  id: string | number;
}>;


export type DeleteFeatureFlagOverrideMutation = { deleteFeatureFlagOverride: { alwaysNil: string | null } };

export type FeatureFlagsQueryVariables = Exact<{ [key: string]: never; }>;


export type FeatureFlagsQuery = { featureFlags: Array<
    | { __typename: 'FeatureFlagBoolean', name: string, value: boolean, createdAt: string, updatedAt: string, managedBy: string | null, overrides: Array<{ id: string, value: boolean, namespace:
          | { __typename: 'Org', name: string, id: string }
          | { __typename: 'User', username: string, id: string }
         }> }
    | { __typename: 'FeatureFlagRollout', name: string, rolloutBasisPoints: number, createdAt: string, updatedAt: string, managedBy: string | null, overrides: Array<{ id: string, value: boolean, namespace:
          | { __typename: 'Org', name: string, id: string }
          | { __typename: 'User', username: string, id: string }
         }> }
  >, site: { declaredConfigFiles: Array<{ source: string, kinds: Array<string>, exclusive: boolean }> } };

export type FeatureFlagNamespaceQueryVariables = Exact<{
  name: string;
}>;


export type FeatureFlagNamespaceQuery = { user: { id: string } | null, organization: { id: string } | null };

export type UpdateFeatureFlagMutationVariables = Exact<{
  name: string;
  value?: boolean | null | undefined;
  rolloutBasisPoints?: number | null | undefined;
}>;


export type UpdateFeatureFlagMutation = { updateFeatureFlag:
    | { __typename: 'FeatureFlagBoolean' }
    | { __typename: 'FeatureFlagRollout' }
   };

export type UpdateFeatureFlagOverrideMutationVariables = Exact<{
  id: string | number;
  value: boolean;
}>;


export type UpdateFeatureFlagOverrideMutation = { updateFeatureFlagOverride: { id: string } };

export type BackgroundJobsQueryVariables = Exact<{
  recentRunCount?: number | null | undefined;
}>;


export type BackgroundJobsQuery = { backgroundJobs: { nodes: Array<{ id: string, name: string, routines: Array<{ name: string, type: BackgroundRoutineType, description: string, intervalMs: number | null, instances: Array<{ hostName: string, lastStartedAt: string | null, lastStoppedAt: string | null }>, recentRuns: Array<{ at: string, hostName: string, durationMs: number, errorMessage: string | null }>, stats: { since: string | null, runCount: number, errorCount: number, minDurationMs: number, avgDurationMs: number, maxDurationMs: number }, queue: { table: string, queued: number, processing: number, completed: number, errored: number, failed: number, canceled: number, oldestQueuedAt: string | null, recentFailures: Array<{ id: string, state: string, failureMessage: string | null, queuedAt: string, startedAt: string | null, finishedAt: string | null, numFailures: number, numResets: number, workerHostname: string }>, activeJobs: Array<{ id: string, state: string, failureMessage: string | null, queuedAt: string, startedAt: string | null, finishedAt: string | null, numFailures: number, numResets: number, workerHostname: string }> } | null }> }> } };

export type CancelBackgroundQueueJobMutationVariables = Exact<{
  queue: string;
  id: string | number;
}>;


export type CancelBackgroundQueueJobMutation = { cancelBackgroundQueueJob: { alwaysNil: string | null } };

export type OutboundRequestsQueryVariables = Exact<{
  first?: number | null | undefined;
  after?: string | null | undefined;
}>;


export type OutboundRequestsQuery = { outboundRequests: { totalCount: number, pageInfo: { hasNextPage: boolean, endCursor: string | null }, nodes: Array<{ id: string, startedAt: string, method: string, url: string, requestBody: string, statusCode: number, durationMs: number, errorMessage: string, creationStackFrame: string, callStack: string, requestHeaders: Array<{ name: string, values: Array<string> }>, responseHeaders: Array<{ name: string, values: Array<string> }> }> } };

export type RetryBackgroundQueueJobMutationVariables = Exact<{
  queue: string;
  id: string | number;
}>;


export type RetryBackgroundQueueJobMutation = { retryBackgroundQueueJob: { alwaysNil: string | null } };

export type SlowRequestsQueryVariables = Exact<{
  first?: number | null | undefined;
  after?: string | null | undefined;
  kind?: SlowRequestKind | null | undefined;
}>;


export type SlowRequestsQuery = { slowRequests: { totalCount: number, pageInfo: { hasNextPage: boolean, endCursor: string | null }, nodes: Array<{ index: string, start: string, duration: number, name: string, source: string, variables: string, errors: Array<string>, query: string, kind: SlowRequestKind, user: { username: string } | null }> } };

export type WebhookLogsQueryVariables = Exact<{
  first?: number | null | undefined;
  after?: string | null | undefined;
  onlyErrors?: boolean | null | undefined;
  onlyUnmatched?: boolean | null | undefined;
  webhookID?: string | number | null | undefined;
}>;


export type WebhookLogsQuery = { webhookLogs: { totalCount: number, pageInfo: { hasNextPage: boolean, endCursor: string | null }, nodes: Array<{ id: string, receivedAt: string, statusCode: number, event: string | null, externalService: { id: string, displayName: string } | null, request: { method: string, url: string, body: string, headers: Array<{ name: string, values: Array<string> }> }, response: { body: string } }> } };

export type ChannelDeliveryLogsQueryVariables = Exact<{
  first?: number | null | undefined;
  after?: string | null | undefined;
  onlyErrors?: boolean | null | undefined;
}>;


export type ChannelDeliveryLogsQuery = { channelDeliveryLogs: { totalCount: number, pageInfo: { hasNextPage: boolean, endCursor: string | null }, nodes: Array<{ id: string, sentAt: string, statusCode: number, error: string | null, channel: DeliveryChannel | null, host: string | null, watchId: string | null, watchDescription: string | null }> } };

export type CreateOutboundWebhookMutationVariables = Exact<{
  input: OutboundWebhookCreateInput;
}>;


export type CreateOutboundWebhookMutation = { createOutboundWebhook: { id: string } };

export type DeleteOutboundWebhookMutationVariables = Exact<{
  id: string | number;
}>;


export type DeleteOutboundWebhookMutation = { deleteOutboundWebhook: { alwaysNil: string | null } };

export type OutboundWebhookQueryVariables = Exact<{
  id: string | number;
  first?: number | null | undefined;
  after?: string | null | undefined;
  onlyErrors?: boolean | null | undefined;
}>;


export type OutboundWebhookQuery = { node:
    | { __typename: 'BackgroundJob' }
    | { __typename: 'BatchChange' }
    | { __typename: 'BatchChangesCredential' }
    | { __typename: 'BatchSpec' }
    | { __typename: 'BulkOperation' }
    | { __typename: 'ChangesetEvent' }
    | { __typename: 'CodeIntelligenceConfigurationPolicy' }
    | { __typename: 'CodeownersIngestedFile' }
    | { __typename: 'ExecutorSecret' }
    | { __typename: 'ExecutorSecretAccessLog' }
    | { __typename: 'ExternalChangeset' }
    | { __typename: 'ExternalService' }
    | { __typename: 'HiddenBatchSpecWorkspace' }
    | { __typename: 'HiddenChangesetSpec' }
    | { __typename: 'HiddenExternalChangeset' }
    | { __typename: 'InsightView' }
    | { __typename: 'InsightsDashboard' }
    | { __typename: 'Monitor' }
    | { __typename: 'Notebook' }
    | { __typename: 'Org' }
    | { __typename: 'OrganizationInvitation' }
    | { __typename: 'OutOfBandMigration' }
    | { __typename: 'OutboundRequest' }
    | { __typename: 'OutboundWebhook', id: string, url: string, createdAt: string, updatedAt: string, managedBy: string | null, eventTypes: Array<{ eventType: string, scope: string | null }>, stats: { total: number, errored: number }, createdBy: { username: string } | null, updatedBy: { username: string } | null, logs: { totalCount: number, pageInfo: { hasNextPage: boolean, endCursor: string | null }, nodes: Array<{ id: string, sentAt: string, statusCode: number, error: string | null, job: { eventType: string, scope: string | null, payload: string }, request: { method: string, url: string, body: string, headers: Array<{ name: string, values: Array<string> }> }, response: { statusCode: number, body: string, headers: Array<{ name: string, values: Array<string> }> } | null }> } }
    | { __typename: 'OutboundWebhookLog' }
    | { __typename: 'PreciseIndex' }
    | { __typename: 'Repository' }
    | { __typename: 'SavedSearch' }
    | { __typename: 'SearchContext' }
    | { __typename: 'SearchJob' }
    | { __typename: 'Team' }
    | { __typename: 'User' }
    | { __typename: 'VisibleBatchSpecWorkspace' }
    | { __typename: 'VisibleChangesetSpec' }
    | { __typename: 'WebhookLog' }
   | null };

export type OutboundWebhookEventTypesQueryVariables = Exact<{ [key: string]: never; }>;


export type OutboundWebhookEventTypesQuery = { outboundWebhookEventTypes: Array<{ key: string, description: string }> };

export type OutboundWebhooksQueryVariables = Exact<{
  first?: number | null | undefined;
  after?: string | null | undefined;
}>;


export type OutboundWebhooksQuery = { outboundWebhooks: { totalCount: number, pageInfo: { hasNextPage: boolean, endCursor: string | null }, nodes: Array<{ id: string, url: string, createdAt: string, updatedAt: string, managedBy: string | null, eventTypes: Array<{ eventType: string, scope: string | null }>, stats: { total: number, errored: number } }> }, site: { declaredConfigFiles: Array<{ source: string, kinds: Array<string>, exclusive: boolean }> } };

export type UpdateOutboundWebhookMutationVariables = Exact<{
  id: string | number;
  input: OutboundWebhookUpdateInput;
}>;


export type UpdateOutboundWebhookMutation = { updateOutboundWebhook: { id: string } };

export type SiteAdminCountsQueryVariables = Exact<{ [key: string]: never; }>;


export type SiteAdminCountsQuery = { accessRequestsCount: number, users: { totalCount: number }, organizations: { totalCount: number }, externalServices: { totalCount: number } };

export type SiteAdminHealthQueryVariables = Exact<{ [key: string]: never; }>;


export type SiteAdminHealthQuery = { site: { productVersion: string }, repositoryStats: { total: number, cloned: number, cloning: number, notCloned: number, queued: number, failedFetch: number, indexed: number, notIndexed: number, corrupted: number, gitDirBytes: string }, gitservers: { totalCount: number, nodes: Array<{ id: string, address: string, freeDiskSpaceBytes: string, totalDiskSpaceBytes: string, error: string | null, clonesPaused: boolean, repositoriesCount: number }> }, statusMessages: Array<
    | { __typename: 'DeclaredConfigError', message: string, source: string, path: string }
    | { __typename: 'GitUpdatesDisabled', message: string }
    | { __typename: 'NoRepositoriesDetected', message: string }
    | { __typename: 'CloningProgress', message: string, cloned: number, cloning: number, queued: number, total: number }
    | { __typename: 'ExternalServiceSyncError', message: string, externalService: { id: string, displayName: string } }
    | { __typename: 'SyncError', message: string }
    | { __typename: 'IndexingProgress', notIndexed: number, indexed: number }
    | { __typename: 'GitserverDiskThresholdReached', message: string, address: string, clonesPaused: boolean }
  > };

export type AnalyticsOverviewQueryVariables = Exact<{
  range: AnalyticsDateRange;
  grouping: AnalyticsGrouping;
}>;


export type AnalyticsOverviewQuery = { site: { analytics: { updatedAt: string | null, users: { activity: { summary: { totalCount: number, totalUniqueUsers: number, totalRegisteredUsers: number }, nodes: Array<{ date: string, count: number, uniqueUsers: number, registeredUsers: number }> }, frequencies: Array<{ daysUsed: number, frequency: number, percentage: number }>, monthlyActiveUsers: Array<{ date: string, count: number }>, signIns: { summary: { totalCount: number, totalUniqueUsers: number, totalRegisteredUsers: number }, nodes: Array<{ date: string, count: number, uniqueUsers: number, registeredUsers: number }> } }, search: { searches: { summary: { totalCount: number, totalUniqueUsers: number, totalRegisteredUsers: number }, nodes: Array<{ date: string, count: number, uniqueUsers: number, registeredUsers: number }> }, resultClicks: { summary: { totalCount: number, totalUniqueUsers: number, totalRegisteredUsers: number }, nodes: Array<{ date: string, count: number, uniqueUsers: number, registeredUsers: number }> }, fileViews: { summary: { totalCount: number, totalUniqueUsers: number, totalRegisteredUsers: number }, nodes: Array<{ date: string, count: number, uniqueUsers: number, registeredUsers: number }> }, searchesByType: Array<{ key: string, count: number }>, latency: { p50: number | null, p90: number | null, p99: number | null } }, codeIntel: { definitionClicks: { summary: { totalCount: number, totalUniqueUsers: number, totalRegisteredUsers: number }, nodes: Array<{ date: string, count: number, uniqueUsers: number, registeredUsers: number }> }, referenceClicks: { summary: { totalCount: number, totalUniqueUsers: number, totalRegisteredUsers: number }, nodes: Array<{ date: string, count: number, uniqueUsers: number, registeredUsers: number }> }, preciseEvents: { summary: { totalCount: number, totalUniqueUsers: number, totalRegisteredUsers: number }, nodes: Array<{ date: string, count: number, uniqueUsers: number, registeredUsers: number }> }, searchBasedEvents: { summary: { totalCount: number, totalUniqueUsers: number, totalRegisteredUsers: number }, nodes: Array<{ date: string, count: number, uniqueUsers: number, registeredUsers: number }> }, crossRepoEvents: { summary: { totalCount: number, totalUniqueUsers: number, totalRegisteredUsers: number }, nodes: Array<{ date: string, count: number, uniqueUsers: number, registeredUsers: number }> } }, codeIntelByLanguage: Array<{ language: string, precision: string, count: number }>, codeIntelTopRepositories: Array<{ name: string, language: string, kind: string, precision: string, events: number, hasPrecise: boolean }>, repos: { count: number, preciseCodeIntelCount: number }, notebooks: { creations: { summary: { totalCount: number, totalUniqueUsers: number, totalRegisteredUsers: number }, nodes: Array<{ date: string, count: number, uniqueUsers: number, registeredUsers: number }> }, views: { summary: { totalCount: number, totalUniqueUsers: number, totalRegisteredUsers: number }, nodes: Array<{ date: string, count: number, uniqueUsers: number, registeredUsers: number }> } }, batchChanges: { changesetsCreated: { summary: { totalCount: number, totalUniqueUsers: number, totalRegisteredUsers: number }, nodes: Array<{ date: string, count: number, uniqueUsers: number, registeredUsers: number }> }, changesetsMerged: { summary: { totalCount: number, totalUniqueUsers: number, totalRegisteredUsers: number }, nodes: Array<{ date: string, count: number, uniqueUsers: number, registeredUsers: number }> }, views: { summary: { totalCount: number, totalUniqueUsers: number, totalRegisteredUsers: number }, nodes: Array<{ date: string, count: number, uniqueUsers: number, registeredUsers: number }> } }, codeInsights: { creations: { summary: { totalCount: number, totalUniqueUsers: number, totalRegisteredUsers: number }, nodes: Array<{ date: string, count: number, uniqueUsers: number, registeredUsers: number }> }, views: { summary: { totalCount: number, totalUniqueUsers: number, totalRegisteredUsers: number }, nodes: Array<{ date: string, count: number, uniqueUsers: number, registeredUsers: number }> } }, codeMonitors: { creations: { summary: { totalCount: number, totalUniqueUsers: number, totalRegisteredUsers: number }, nodes: Array<{ date: string, count: number, uniqueUsers: number, registeredUsers: number }> }, views: { summary: { totalCount: number, totalUniqueUsers: number, totalRegisteredUsers: number }, nodes: Array<{ date: string, count: number, uniqueUsers: number, registeredUsers: number }> } } }, usageStatistics: { daus: Array<{ userCount: number }>, waus: Array<{ userCount: number }>, maus: Array<{ userCount: number }> } } };

export type SiteUsersActivityQueryVariables = Exact<{
  query?: string | null | undefined;
  lastActiveAt?: SiteUsersDateRangeInput | null | undefined;
  limit: number;
  offset: number;
  orderBy: SiteUserOrderBy;
  descending: boolean;
}>;


export type SiteUsersActivityQuery = { site: { users: { totalCount: number, nodes: Array<{ id: string, username: string, displayName: string | null, email: string | null, siteAdmin: boolean, createdAt: string, lastActiveAt: string | null, eventsCount: number, locked: boolean }> } } };

export type BatchChangeDetailQueryVariables = Exact<{
  namespace: string | number;
  name: string;
}>;


export type BatchChangeDetailQuery = { batchChange: { id: string, name: string, description: string | null, state: BatchChangeState, url: string, viewerCanAdminister: boolean, createdAt: string, updatedAt: string, lastAppliedAt: string | null, closedAt: string | null, namespace:
      | { __typename: 'Org', name: string, id: string, url: string }
      | { __typename: 'User', username: string, id: string, url: string }
    , creator: { username: string, url: string } | null, lastApplier: { username: string, url: string } | null, diffStat: { added: number, deleted: number }, changesetsStats: { unpublished: number, draft: number, open: number, merged: number, closed: number, deleted: number, retrying: number, failed: number, scheduled: number, processing: number, archived: number, total: number, isCompleted: boolean, percentComplete: number }, currentSpec: { id: string, originalInput: string, createdAt: string, source: BatchSpecSource, applyURL: string | null }, bulkOperations: { totalCount: number } } | null };

export type BatchChangesQueryVariables = Exact<{
  first?: number | null | undefined;
  after?: string | null | undefined;
  states?: Array<BatchChangeState> | BatchChangeState | null | undefined;
}>;


export type BatchChangesQuery = { batchChanges: { totalCount: number, pageInfo: { hasNextPage: boolean, endCursor: string | null }, nodes: Array<{ id: string, name: string, description: string | null, state: BatchChangeState, url: string, createdAt: string, closedAt: string | null, lastAppliedAt: string | null, namespace:
        | { __typename: 'Org', name: string, id: string, url: string }
        | { __typename: 'User', username: string, id: string, url: string }
      , creator: { username: string } | null, changesetsStats: { unpublished: number, draft: number, open: number, merged: number, closed: number, deleted: number, retrying: number, failed: number, scheduled: number, processing: number, archived: number, total: number, isCompleted: boolean, percentComplete: number } }> } };

export type BulkOperationsQueryVariables = Exact<{
  batchChange: string | number;
  first?: number | null | undefined;
  after?: string | null | undefined;
}>;


export type BulkOperationsQuery = { node:
    | { bulkOperations: { totalCount: number, pageInfo: { hasNextPage: boolean, endCursor: string | null }, nodes: Array<{ id: string, type: BulkOperationType, state: BulkOperationState, progress: number, changesetCount: number, createdAt: string, finishedAt: string | null, initiator: { username: string }, errors: Array<{ error: string | null, changeset:
              | { __typename: 'ExternalChangeset', title: string | null, id: string, repository: { name: string } }
              | { __typename: 'HiddenExternalChangeset', id: string }
             }> }> } }
    | Record<PropertyKey, never>
   | null };

export type ChangesetCountsOverTimeQueryVariables = Exact<{
  batchChange: string | number;
  from?: string | null | undefined;
  includeArchived?: boolean | null | undefined;
}>;


export type ChangesetCountsOverTimeQuery = { node:
    | { changesetCountsOverTime: Array<{ date: string, total: number, merged: number, closed: number, draft: number, open: number, openApproved: number, openChangesRequested: number, openPending: number }> }
    | Record<PropertyKey, never>
   | null };

export type CloseBatchChangeMutationVariables = Exact<{
  batchChange: string | number;
  closeChangesets?: boolean | null | undefined;
}>;


export type CloseBatchChangeMutation = { closeBatchChange: { id: string, state: BatchChangeState, closedAt: string | null } };

export type DeleteBatchChangeMutationVariables = Exact<{
  batchChange: string | number;
}>;


export type DeleteBatchChangeMutation = { deleteBatchChange: { alwaysNil: string | null } | null };

export type OrgBatchChangesQueryVariables = Exact<{
  org: string;
  first?: number | null | undefined;
  after?: string | null | undefined;
  states?: Array<BatchChangeState> | BatchChangeState | null | undefined;
}>;


export type OrgBatchChangesQuery = { organization: { id: string, batchChanges: { totalCount: number, pageInfo: { hasNextPage: boolean, endCursor: string | null }, nodes: Array<{ id: string, name: string, description: string | null, state: BatchChangeState, url: string, createdAt: string, closedAt: string | null, lastAppliedAt: string | null, namespace:
          | { __typename: 'Org', name: string, id: string, url: string }
          | { __typename: 'User', username: string, id: string, url: string }
        , creator: { username: string } | null, changesetsStats: { unpublished: number, draft: number, open: number, merged: number, closed: number, deleted: number, retrying: number, failed: number, scheduled: number, processing: number, archived: number, total: number, isCompleted: boolean, percentComplete: number } }> } } | null };

export type BatchChangeOrgIdQueryVariables = Exact<{
  name: string;
}>;


export type BatchChangeOrgIdQuery = { organization: { id: string } | null };

export type UserBatchChangesQueryVariables = Exact<{
  user: string;
  first?: number | null | undefined;
  after?: string | null | undefined;
  states?: Array<BatchChangeState> | BatchChangeState | null | undefined;
}>;


export type UserBatchChangesQuery = { user: { id: string, batchChanges: { totalCount: number, pageInfo: { hasNextPage: boolean, endCursor: string | null }, nodes: Array<{ id: string, name: string, description: string | null, state: BatchChangeState, url: string, createdAt: string, closedAt: string | null, lastAppliedAt: string | null, namespace:
          | { __typename: 'Org', name: string, id: string, url: string }
          | { __typename: 'User', username: string, id: string, url: string }
        , creator: { username: string } | null, changesetsStats: { unpublished: number, draft: number, open: number, merged: number, closed: number, deleted: number, retrying: number, failed: number, scheduled: number, processing: number, archived: number, total: number, isCompleted: boolean, percentComplete: number } }> } } | null };

export type BatchChangeUserIdQueryVariables = Exact<{
  name: string;
}>;


export type BatchChangeUserIdQuery = { user: { id: string } | null };

export type AvailableBulkOperationsQueryVariables = Exact<{
  batchChange: string | number;
  changesets: Array<string | number> | string | number;
}>;


export type AvailableBulkOperationsQuery = { node:
    | { availableBulkOperations: Array<BulkOperationType> }
    | Record<PropertyKey, never>
   | null };

export type BatchChangeChangesetIDsQueryVariables = Exact<{
  batchChange: string | number;
  first?: number | null | undefined;
  state?: ChangesetState | null | undefined;
  reviewState?: ChangesetReviewState | null | undefined;
  checkState?: ChangesetCheckState | null | undefined;
  search?: string | null | undefined;
  onlyArchived?: boolean | null | undefined;
}>;


export type BatchChangeChangesetIDsQuery = { node:
    | { changesets: { totalCount: number, nodes: Array<
          | { __typename: 'ExternalChangeset', id: string }
          | { __typename: 'HiddenExternalChangeset', id: string }
        > } }
    | Record<PropertyKey, never>
   | null };

export type ChangesetSpecDiffQueryVariables = Exact<{
  id: string | number;
  first?: number | null | undefined;
}>;


export type ChangesetSpecDiffQuery = { node:
    | { description:
        | { __typename: 'ExistingChangesetReference' }
        | { __typename: 'GitBranchChangesetDescription', diff: { fileDiffs: { totalCount: number, diffStat: { added: number, deleted: number }, nodes: Array<{ oldPath: string | null, newPath: string | null, binary: boolean, mostRelevantFile:
                  | { __typename: 'GitBlob', url: string }
                  | { __typename: 'VirtualFile', url: string }
                 | null, stat: { added: number, deleted: number }, hunks: Array<{ body: string, oldRange: { startLine: number, lines: number }, newRange: { startLine: number, lines: number } }> }> } } }
       }
    | Record<PropertyKey, never>
   | null };

export type BatchChangeChangesetsQueryVariables = Exact<{
  batchChange: string | number;
  first?: number | null | undefined;
  after?: string | null | undefined;
  state?: ChangesetState | null | undefined;
  reviewState?: ChangesetReviewState | null | undefined;
  checkState?: ChangesetCheckState | null | undefined;
  search?: string | null | undefined;
  onlyArchived?: boolean | null | undefined;
}>;


export type BatchChangeChangesetsQuery = { node:
    | { changesets: { totalCount: number, pageInfo: { hasNextPage: boolean, endCursor: string | null }, nodes: Array<
          | { __typename: 'ExternalChangeset', title: string | null, body: string | null, externalID: string | null, reviewState: ChangesetReviewState | null, checkState: ChangesetCheckState | null, error: string | null, syncerError: string | null, scheduleEstimateAt: string | null, forkNamespace: string | null, id: string, state: ChangesetState, createdAt: string, updatedAt: string, nextSyncAt: string | null, externalURL: { url: string } | null, repository: { id: string, name: string, url: string }, diffStat: { added: number, deleted: number } | null, labels: Array<{ text: string, color: string }>, currentSpec: { id: string, description:
                | { __typename: 'ExistingChangesetReference' }
                | { __typename: 'GitBranchChangesetDescription', baseRef: string, headRef: string, published: boolean | 'draft' | null }
               } | null }
          | { __typename: 'HiddenExternalChangeset', id: string, state: ChangesetState, createdAt: string, updatedAt: string, nextSyncAt: string | null }
        > } }
    | Record<PropertyKey, never>
   | null };

export type ChangesetsCsvQueryVariables = Exact<{
  batchChange: string | number;
  changesets?: Array<string | number> | string | number | null | undefined;
  state?: ChangesetState | null | undefined;
  reviewState?: ChangesetReviewState | null | undefined;
  checkState?: ChangesetCheckState | null | undefined;
  search?: string | null | undefined;
  onlyArchived?: boolean | null | undefined;
}>;


export type ChangesetsCsvQuery = { node:
    | { changesetsCSV: string }
    | Record<PropertyKey, never>
   | null };

export type CloseChangesetsMutationVariables = Exact<{
  batchChange: string | number;
  changesets: Array<string | number> | string | number;
}>;


export type CloseChangesetsMutation = { closeChangesets: { id: string } };

export type CreateChangesetCommentsMutationVariables = Exact<{
  batchChange: string | number;
  changesets: Array<string | number> | string | number;
  body: string;
}>;


export type CreateChangesetCommentsMutation = { createChangesetComments: { id: string } };

export type DetachChangesetsMutationVariables = Exact<{
  batchChange: string | number;
  changesets: Array<string | number> | string | number;
}>;


export type DetachChangesetsMutation = { detachChangesets: { id: string } };

export type MergeChangesetsMutationVariables = Exact<{
  batchChange: string | number;
  changesets: Array<string | number> | string | number;
  squash?: boolean | null | undefined;
}>;


export type MergeChangesetsMutation = { mergeChangesets: { id: string } };

export type PublishChangesetsMutationVariables = Exact<{
  batchChange: string | number;
  changesets: Array<string | number> | string | number;
  draft?: boolean | null | undefined;
}>;


export type PublishChangesetsMutation = { publishChangesets: { id: string } };

export type ReenqueueChangesetsMutationVariables = Exact<{
  batchChange: string | number;
  changesets: Array<string | number> | string | number;
}>;


export type ReenqueueChangesetsMutation = { reenqueueChangesets: { id: string } };

export type CreateBatchChangesCredentialMutationVariables = Exact<{
  user?: string | number | null | undefined;
  externalServiceKind: ExternalServiceKind;
  externalServiceURL: string;
  username?: string | null | undefined;
  credential: string;
}>;


export type CreateBatchChangesCredentialMutation = { createBatchChangesCredential: { id: string } };

export type DeleteBatchChangesCredentialMutationVariables = Exact<{
  id: string | number;
}>;


export type DeleteBatchChangesCredentialMutation = { deleteBatchChangesCredential: { alwaysNil: string | null } };

export type SiteBatchChangesCodeHostsQueryVariables = Exact<{
  first?: number | null | undefined;
}>;


export type SiteBatchChangesCodeHostsQuery = { batchChangesCodeHosts: { totalCount: number, nodes: Array<{ externalServiceKind: ExternalServiceKind, externalServiceURL: string, requiresSSH: boolean, requiresUsername: boolean, hasWebhooks: boolean, credential: { id: string, isSiteCredential: boolean, createdAt: string, sshPublicKey: string | null } | null }> } };

export type UserBatchChangesCodeHostsQueryVariables = Exact<{
  first?: number | null | undefined;
}>;


export type UserBatchChangesCodeHostsQuery = { currentUser: { id: string, batchChangesCodeHosts: { totalCount: number, nodes: Array<{ externalServiceKind: ExternalServiceKind, externalServiceURL: string, requiresSSH: boolean, requiresUsername: boolean, hasWebhooks: boolean, credential: { id: string, isSiteCredential: boolean, createdAt: string, sshPublicKey: string | null } | null }> } } | null };

export type BatchSpecExecutionQueryVariables = Exact<{
  id: string | number;
  first?: number | null | undefined;
  search?: string | null | undefined;
  state?: BatchSpecWorkspaceState | null | undefined;
}>;


export type BatchSpecExecutionQuery = { node:
    | { __typename: 'BackgroundJob' }
    | { __typename: 'BatchChange' }
    | { __typename: 'BatchChangesCredential' }
    | { __typename: 'BatchSpec', id: string, state: BatchSpecState, source: BatchSpecSource, originalInput: string, createdAt: string, startedAt: string | null, finishedAt: string | null, applyURL: string | null, failureMessage: string | null, viewerCanAdminister: boolean, viewerCanRetry: boolean, noCache: boolean | null, description: { name: string, description: string }, creator: { id: string, username: string } | null, namespace:
        | { id: string, url: string }
        | { id: string, url: string }
      , diffStat: { added: number, deleted: number } | null, workspaceResolution: { state: BatchSpecWorkspaceResolutionState, failureMessage: string | null, all: { totalCount: number, stats: { errored: number, completed: number, processing: number, queued: number, ignored: number } }, skipped: { totalCount: number }, workspaces: { totalCount: number, pageInfo: { hasNextPage: boolean, endCursor: string | null }, nodes: Array<
            | { __typename: 'HiddenBatchSpecWorkspace', id: string, state: BatchSpecWorkspaceState, ignored: boolean, unsupported: boolean, cachedResultFound: boolean, stepCacheResultCount: number, queuedAt: string | null, startedAt: string | null, finishedAt: string | null, placeInQueue: number | null, placeInGlobalQueue: number | null, diffStat: { added: number, deleted: number } | null }
            | { __typename: 'VisibleBatchSpecWorkspace', path: string, id: string, state: BatchSpecWorkspaceState, ignored: boolean, unsupported: boolean, cachedResultFound: boolean, stepCacheResultCount: number, queuedAt: string | null, startedAt: string | null, finishedAt: string | null, placeInQueue: number | null, placeInGlobalQueue: number | null, repository: { name: string, url: string }, branch: { displayName: string }, diffStat: { added: number, deleted: number } | null }
          > } } | null }
    | { __typename: 'BulkOperation' }
    | { __typename: 'ChangesetEvent' }
    | { __typename: 'CodeIntelligenceConfigurationPolicy' }
    | { __typename: 'CodeownersIngestedFile' }
    | { __typename: 'ExecutorSecret' }
    | { __typename: 'ExecutorSecretAccessLog' }
    | { __typename: 'ExternalChangeset' }
    | { __typename: 'ExternalService' }
    | { __typename: 'HiddenBatchSpecWorkspace' }
    | { __typename: 'HiddenChangesetSpec' }
    | { __typename: 'HiddenExternalChangeset' }
    | { __typename: 'InsightView' }
    | { __typename: 'InsightsDashboard' }
    | { __typename: 'Monitor' }
    | { __typename: 'Notebook' }
    | { __typename: 'Org' }
    | { __typename: 'OrganizationInvitation' }
    | { __typename: 'OutOfBandMigration' }
    | { __typename: 'OutboundRequest' }
    | { __typename: 'OutboundWebhook' }
    | { __typename: 'OutboundWebhookLog' }
    | { __typename: 'PreciseIndex' }
    | { __typename: 'Repository' }
    | { __typename: 'SavedSearch' }
    | { __typename: 'SearchContext' }
    | { __typename: 'SearchJob' }
    | { __typename: 'Team' }
    | { __typename: 'User' }
    | { __typename: 'VisibleBatchSpecWorkspace' }
    | { __typename: 'VisibleChangesetSpec' }
    | { __typename: 'WebhookLog' }
   | null };

type ExecutionWorkspaceFields_HiddenBatchSpecWorkspace_Fragment = { __typename: 'HiddenBatchSpecWorkspace', id: string, state: BatchSpecWorkspaceState, ignored: boolean, unsupported: boolean, cachedResultFound: boolean, stepCacheResultCount: number, queuedAt: string | null, startedAt: string | null, finishedAt: string | null, placeInQueue: number | null, placeInGlobalQueue: number | null, diffStat: { added: number, deleted: number } | null };

type ExecutionWorkspaceFields_VisibleBatchSpecWorkspace_Fragment = { __typename: 'VisibleBatchSpecWorkspace', path: string, id: string, state: BatchSpecWorkspaceState, ignored: boolean, unsupported: boolean, cachedResultFound: boolean, stepCacheResultCount: number, queuedAt: string | null, startedAt: string | null, finishedAt: string | null, placeInQueue: number | null, placeInGlobalQueue: number | null, repository: { name: string, url: string }, branch: { displayName: string }, diffStat: { added: number, deleted: number } | null };

export type ExecutionWorkspaceFieldsFragment =
  | ExecutionWorkspaceFields_HiddenBatchSpecWorkspace_Fragment
  | ExecutionWorkspaceFields_VisibleBatchSpecWorkspace_Fragment
;

export type BatchSpecWorkspaceQueryVariables = Exact<{
  id: string | number;
}>;


export type BatchSpecWorkspaceQuery = { node:
    | { __typename: 'BackgroundJob' }
    | { __typename: 'BatchChange' }
    | { __typename: 'BatchChangesCredential' }
    | { __typename: 'BatchSpec' }
    | { __typename: 'BulkOperation' }
    | { __typename: 'ChangesetEvent' }
    | { __typename: 'CodeIntelligenceConfigurationPolicy' }
    | { __typename: 'CodeownersIngestedFile' }
    | { __typename: 'ExecutorSecret' }
    | { __typename: 'ExecutorSecretAccessLog' }
    | { __typename: 'ExternalChangeset' }
    | { __typename: 'ExternalService' }
    | { __typename: 'HiddenBatchSpecWorkspace', id: string, state: BatchSpecWorkspaceState, ignored: boolean, unsupported: boolean, cachedResultFound: boolean, stepCacheResultCount: number, queuedAt: string | null, startedAt: string | null, finishedAt: string | null, placeInQueue: number | null, placeInGlobalQueue: number | null, diffStat: { added: number, deleted: number } | null }
    | { __typename: 'HiddenChangesetSpec' }
    | { __typename: 'HiddenExternalChangeset' }
    | { __typename: 'InsightView' }
    | { __typename: 'InsightsDashboard' }
    | { __typename: 'Monitor' }
    | { __typename: 'Notebook' }
    | { __typename: 'Org' }
    | { __typename: 'OrganizationInvitation' }
    | { __typename: 'OutOfBandMigration' }
    | { __typename: 'OutboundRequest' }
    | { __typename: 'OutboundWebhook' }
    | { __typename: 'OutboundWebhookLog' }
    | { __typename: 'PreciseIndex' }
    | { __typename: 'Repository' }
    | { __typename: 'SavedSearch' }
    | { __typename: 'SearchContext' }
    | { __typename: 'SearchJob' }
    | { __typename: 'Team' }
    | { __typename: 'User' }
    | { __typename: 'VisibleBatchSpecWorkspace', onlyFetchWorkspace: boolean, searchResultPaths: Array<string>, failureMessage: string | null, path: string, id: string, state: BatchSpecWorkspaceState, ignored: boolean, unsupported: boolean, cachedResultFound: boolean, stepCacheResultCount: number, queuedAt: string | null, startedAt: string | null, finishedAt: string | null, placeInQueue: number | null, placeInGlobalQueue: number | null, executor: { hostname: string } | null, stages: { setup: Array<{ key: string, command: Array<string>, startTime: string | null, exitCode: number | null, out: string, durationMilliseconds: number | null }>, srcExec: Array<{ key: string, command: Array<string>, startTime: string | null, exitCode: number | null, out: string, durationMilliseconds: number | null }>, teardown: Array<{ key: string, command: Array<string>, startTime: string | null, exitCode: number | null, out: string, durationMilliseconds: number | null }> } | null, steps: Array<{ number: number, run: string, container: string, ifCondition: string | null, cachedResultFound: boolean, skipped: boolean, startedAt: string | null, finishedAt: string | null, exitCode: number | null, environment: Array<{ name: string, value: string | null }>, outputVariables: Array<{ name: string, value: unknown }> | null, diffStat: { added: number, deleted: number } | null, outputLines: { totalCount: number, nodes: Array<string> } }>, changesetSpecs: Array<{ id: string, description:
          | { __typename: 'ExistingChangesetReference' }
          | { __typename: 'GitBranchChangesetDescription', title: string, headRef: string, diffStat: { added: number, deleted: number } }
         }> | null, repository: { name: string, url: string }, branch: { displayName: string }, diffStat: { added: number, deleted: number } | null }
    | { __typename: 'VisibleChangesetSpec' }
    | { __typename: 'WebhookLog' }
   | null };

export type ExecutionLogFieldsFragment = { key: string, command: Array<string>, startTime: string | null, exitCode: number | null, out: string, durationMilliseconds: number | null };

export type CancelBatchSpecExecutionMutationVariables = Exact<{
  batchSpec: string | number;
}>;


export type CancelBatchSpecExecutionMutation = { cancelBatchSpecExecution: { id: string, state: BatchSpecState } };

export type RetryBatchSpecExecutionMutationVariables = Exact<{
  batchSpec: string | number;
  includeCompleted?: boolean | null | undefined;
}>;


export type RetryBatchSpecExecutionMutation = { retryBatchSpecExecution: { id: string, state: BatchSpecState } };

export type RetryBatchSpecWorkspaceExecutionMutationVariables = Exact<{
  workspaces: Array<string | number> | string | number;
}>;


export type RetryBatchSpecWorkspaceExecutionMutation = { retryBatchSpecWorkspaceExecution: { alwaysNil: string | null } };

export type ApplyBatchChangeFromUiMutationVariables = Exact<{
  batchSpec: string | number;
  ensureBatchChange?: string | number | null | undefined;
  publicationStates?: Array<ChangesetSpecPublicationStateInput> | ChangesetSpecPublicationStateInput | null | undefined;
}>;


export type ApplyBatchChangeFromUiMutation = { applyBatchChange: { id: string, url: string } };

export type BatchSpecApplyPreviewQueryVariables = Exact<{
  batchSpec: string | number;
  first?: number | null | undefined;
  after?: string | null | undefined;
  search?: string | null | undefined;
  currentState?: ChangesetState | null | undefined;
  action?: ChangesetSpecOperation | null | undefined;
  publicationStates?: Array<ChangesetSpecPublicationStateInput> | ChangesetSpecPublicationStateInput | null | undefined;
}>;


export type BatchSpecApplyPreviewQuery = { node:
    | { applyPreview: { totalCount: number, pageInfo: { hasNextPage: boolean, endCursor: string | null }, stats: { push: number, update: number, undraft: number, publish: number, publishDraft: number, sync: number, import: number, close: number, reopen: number, sleep: number, detach: number, archive: number, reattach: number, added: number, modified: number, removed: number }, nodes: Array<
          | { __typename: 'VisibleChangesetApplyPreview', operations: Array<ChangesetSpecOperation>, delta: { titleChanged: boolean, bodyChanged: boolean, undraft: boolean, baseRefChanged: boolean, diffChanged: boolean, commitMessageChanged: boolean, authorNameChanged: boolean, authorEmailChanged: boolean }, targets:
              | { __typename: 'VisibleApplyPreviewTargetsAttach', changesetSpec: { id: string, type: ChangesetSpecType, description:
                    | { __typename: 'ExistingChangesetReference', externalID: string, baseRepository: { name: string, url: string } }
                    | { __typename: 'GitBranchChangesetDescription', baseRef: string, headRef: string, title: string, body: string, published: boolean | 'draft' | null, baseRepository: { name: string, url: string }, diffStat: { added: number, deleted: number } }
                   } }
              | { __typename: 'VisibleApplyPreviewTargetsUpdate', changesetSpec: { id: string, type: ChangesetSpecType, description:
                    | { __typename: 'ExistingChangesetReference', externalID: string, baseRepository: { name: string, url: string } }
                    | { __typename: 'GitBranchChangesetDescription', baseRef: string, headRef: string, title: string, body: string, published: boolean | 'draft' | null, baseRepository: { name: string, url: string }, diffStat: { added: number, deleted: number } }
                   }, changeset: { id: string, state: ChangesetState, title: string | null, externalID: string | null, externalURL: { url: string } | null, repository: { name: string, url: string } } }
              | { __typename: 'VisibleApplyPreviewTargetsDetach', changeset: { id: string, state: ChangesetState, title: string | null, externalID: string | null, externalURL: { url: string } | null, repository: { name: string, url: string } } }
             }
          | { __typename: 'HiddenChangesetApplyPreview', operations: Array<ChangesetSpecOperation>, targets:
              | { __typename: 'HiddenApplyPreviewTargetsAttach' }
              | { __typename: 'HiddenApplyPreviewTargetsUpdate' }
              | { __typename: 'HiddenApplyPreviewTargetsDetach' }
             }
        > } }
    | Record<PropertyKey, never>
   | null };

export type BatchSpecForApplyQueryVariables = Exact<{
  id: string | number;
}>;


export type BatchSpecForApplyQuery = { node:
    | { __typename: 'BackgroundJob' }
    | { __typename: 'BatchChange' }
    | { __typename: 'BatchChangesCredential' }
    | { __typename: 'BatchSpec', id: string, state: BatchSpecState, source: BatchSpecSource, createdAt: string, expiresAt: string | null, applyURL: string | null, viewerCanAdminister: boolean, description: { name: string, description: string }, namespace:
        | { __typename: 'Org', name: string, id: string, url: string }
        | { __typename: 'User', username: string, id: string, url: string }
      , creator: { username: string } | null, diffStat: { added: number, deleted: number } | null, appliesToBatchChange: { id: string, name: string, url: string, state: BatchChangeState } | null, changesetSpecs: { totalCount: number } }
    | { __typename: 'BulkOperation' }
    | { __typename: 'ChangesetEvent' }
    | { __typename: 'CodeIntelligenceConfigurationPolicy' }
    | { __typename: 'CodeownersIngestedFile' }
    | { __typename: 'ExecutorSecret' }
    | { __typename: 'ExecutorSecretAccessLog' }
    | { __typename: 'ExternalChangeset' }
    | { __typename: 'ExternalService' }
    | { __typename: 'HiddenBatchSpecWorkspace' }
    | { __typename: 'HiddenChangesetSpec' }
    | { __typename: 'HiddenExternalChangeset' }
    | { __typename: 'InsightView' }
    | { __typename: 'InsightsDashboard' }
    | { __typename: 'Monitor' }
    | { __typename: 'Notebook' }
    | { __typename: 'Org' }
    | { __typename: 'OrganizationInvitation' }
    | { __typename: 'OutOfBandMigration' }
    | { __typename: 'OutboundRequest' }
    | { __typename: 'OutboundWebhook' }
    | { __typename: 'OutboundWebhookLog' }
    | { __typename: 'PreciseIndex' }
    | { __typename: 'Repository' }
    | { __typename: 'SavedSearch' }
    | { __typename: 'SearchContext' }
    | { __typename: 'SearchJob' }
    | { __typename: 'Team' }
    | { __typename: 'User' }
    | { __typename: 'VisibleBatchSpecWorkspace' }
    | { __typename: 'VisibleChangesetSpec' }
    | { __typename: 'WebhookLog' }
   | null };

export type CreateBatchChangeFromUiMutationVariables = Exact<{
  batchSpec: string | number;
  publicationStates?: Array<ChangesetSpecPublicationStateInput> | ChangesetSpecPublicationStateInput | null | undefined;
}>;


export type CreateBatchChangeFromUiMutation = { createBatchChange: { id: string, url: string } };

export type BatchChangeForEditQueryVariables = Exact<{
  namespace: string | number;
  name: string;
}>;


export type BatchChangeForEditQuery = { batchChange: { id: string, name: string, state: BatchChangeState, url: string, viewerCanAdminister: boolean, currentSpec: { id: string, state: BatchSpecState, source: BatchSpecSource, originalInput: string, createdAt: string, startedAt: string | null, finishedAt: string | null, applyURL: string | null, viewerCanAdminister: boolean, allowIgnored: boolean | null, allowUnsupported: boolean | null, noCache: boolean | null, failureMessage: string | null, creator: { id: string, username: string } | null, workspaceResolution: { state: BatchSpecWorkspaceResolutionState, failureMessage: string | null, startedAt: string | null, finishedAt: string | null, workspaces: { totalCount: number, stats: { errored: number, completed: number, processing: number, queued: number, ignored: number } }, skipped: { totalCount: number } } | null }, batchSpecs: { nodes: Array<{ id: string, state: BatchSpecState, source: BatchSpecSource, originalInput: string, createdAt: string, startedAt: string | null, finishedAt: string | null, applyURL: string | null, viewerCanAdminister: boolean, allowIgnored: boolean | null, allowUnsupported: boolean | null, noCache: boolean | null, failureMessage: string | null, creator: { id: string, username: string } | null, workspaceResolution: { state: BatchSpecWorkspaceResolutionState, failureMessage: string | null, startedAt: string | null, finishedAt: string | null, workspaces: { totalCount: number, stats: { errored: number, completed: number, processing: number, queued: number, ignored: number } }, skipped: { totalCount: number } } | null }> } } | null };

export type SpecEditorFieldsFragment = { id: string, state: BatchSpecState, source: BatchSpecSource, originalInput: string, createdAt: string, startedAt: string | null, finishedAt: string | null, applyURL: string | null, viewerCanAdminister: boolean, allowIgnored: boolean | null, allowUnsupported: boolean | null, noCache: boolean | null, failureMessage: string | null, creator: { id: string, username: string } | null, workspaceResolution: { state: BatchSpecWorkspaceResolutionState, failureMessage: string | null, startedAt: string | null, finishedAt: string | null, workspaces: { totalCount: number, stats: { errored: number, completed: number, processing: number, queued: number, ignored: number } }, skipped: { totalCount: number } } | null };

export type BatchesProbeQueryVariables = Exact<{ [key: string]: never; }>;


export type BatchesProbeQuery = { batchChanges: { totalCount: number } };

export type BatchesViewerQueryVariables = Exact<{ [key: string]: never; }>;


export type BatchesViewerQuery = { currentUser: { id: string, username: string, siteAdmin: boolean, url: string, permissions: { nodes: Array<{ namespace: string, action: string }> }, organizations: { nodes: Array<{ id: string, name: string, displayName: string | null, url: string }> } } | null };

export type CreateBatchSpecFromRawMutationVariables = Exact<{
  spec: string;
  namespace: string | number;
  batchChange: string | number;
  allowIgnored?: boolean | null | undefined;
  allowUnsupported?: boolean | null | undefined;
  noCache?: boolean | null | undefined;
}>;


export type CreateBatchSpecFromRawMutation = { createBatchSpecFromRaw: { id: string, state: BatchSpecState, source: BatchSpecSource, originalInput: string, createdAt: string, startedAt: string | null, finishedAt: string | null, applyURL: string | null, viewerCanAdminister: boolean, allowIgnored: boolean | null, allowUnsupported: boolean | null, noCache: boolean | null, failureMessage: string | null, creator: { id: string, username: string } | null, workspaceResolution: { state: BatchSpecWorkspaceResolutionState, failureMessage: string | null, startedAt: string | null, finishedAt: string | null, workspaces: { totalCount: number, stats: { errored: number, completed: number, processing: number, queued: number, ignored: number } }, skipped: { totalCount: number } } | null } };

export type CreateEmptyBatchChangeMutationVariables = Exact<{
  namespace: string | number;
  name: string;
}>;


export type CreateEmptyBatchChangeMutation = { createEmptyBatchChange: { id: string, name: string, url: string } };

export type EditorSpecQueryVariables = Exact<{
  id: string | number;
}>;


export type EditorSpecQuery = { node:
    | { __typename: 'BackgroundJob' }
    | { __typename: 'BatchChange' }
    | { __typename: 'BatchChangesCredential' }
    | { __typename: 'BatchSpec', id: string, state: BatchSpecState, source: BatchSpecSource, originalInput: string, createdAt: string, startedAt: string | null, finishedAt: string | null, applyURL: string | null, viewerCanAdminister: boolean, allowIgnored: boolean | null, allowUnsupported: boolean | null, noCache: boolean | null, failureMessage: string | null, creator: { id: string, username: string } | null, workspaceResolution: { state: BatchSpecWorkspaceResolutionState, failureMessage: string | null, startedAt: string | null, finishedAt: string | null, workspaces: { totalCount: number, stats: { errored: number, completed: number, processing: number, queued: number, ignored: number } }, skipped: { totalCount: number } } | null }
    | { __typename: 'BulkOperation' }
    | { __typename: 'ChangesetEvent' }
    | { __typename: 'CodeIntelligenceConfigurationPolicy' }
    | { __typename: 'CodeownersIngestedFile' }
    | { __typename: 'ExecutorSecret' }
    | { __typename: 'ExecutorSecretAccessLog' }
    | { __typename: 'ExternalChangeset' }
    | { __typename: 'ExternalService' }
    | { __typename: 'HiddenBatchSpecWorkspace' }
    | { __typename: 'HiddenChangesetSpec' }
    | { __typename: 'HiddenExternalChangeset' }
    | { __typename: 'InsightView' }
    | { __typename: 'InsightsDashboard' }
    | { __typename: 'Monitor' }
    | { __typename: 'Notebook' }
    | { __typename: 'Org' }
    | { __typename: 'OrganizationInvitation' }
    | { __typename: 'OutOfBandMigration' }
    | { __typename: 'OutboundRequest' }
    | { __typename: 'OutboundWebhook' }
    | { __typename: 'OutboundWebhookLog' }
    | { __typename: 'PreciseIndex' }
    | { __typename: 'Repository' }
    | { __typename: 'SavedSearch' }
    | { __typename: 'SearchContext' }
    | { __typename: 'SearchJob' }
    | { __typename: 'Team' }
    | { __typename: 'User' }
    | { __typename: 'VisibleBatchSpecWorkspace' }
    | { __typename: 'VisibleChangesetSpec' }
    | { __typename: 'WebhookLog' }
   | null };

export type ExecuteBatchSpecMutationVariables = Exact<{
  batchSpec: string | number;
  noCache?: boolean | null | undefined;
}>;


export type ExecuteBatchSpecMutation = { executeBatchSpec: { id: string, state: BatchSpecState } };

export type BatchOrgNamespaceQueryVariables = Exact<{
  name: string;
}>;


export type BatchOrgNamespaceQuery = { organization: { id: string, url: string } | null };

export type ReplaceBatchSpecInputMutationVariables = Exact<{
  previousSpec: string | number;
  spec: string;
  allowIgnored?: boolean | null | undefined;
  allowUnsupported?: boolean | null | undefined;
  noCache?: boolean | null | undefined;
}>;


export type ReplaceBatchSpecInputMutation = { replaceBatchSpecInput: { id: string, state: BatchSpecState, source: BatchSpecSource, originalInput: string, createdAt: string, startedAt: string | null, finishedAt: string | null, applyURL: string | null, viewerCanAdminister: boolean, allowIgnored: boolean | null, allowUnsupported: boolean | null, noCache: boolean | null, failureMessage: string | null, creator: { id: string, username: string } | null, workspaceResolution: { state: BatchSpecWorkspaceResolutionState, failureMessage: string | null, startedAt: string | null, finishedAt: string | null, workspaces: { totalCount: number, stats: { errored: number, completed: number, processing: number, queued: number, ignored: number } }, skipped: { totalCount: number } } | null } };

export type BatchUserNamespaceQueryVariables = Exact<{
  name: string;
}>;


export type BatchUserNamespaceQuery = { user: { id: string, url: string } | null };

export type WorkspacesPreviewQueryVariables = Exact<{
  id: string | number;
  first?: number | null | undefined;
  after?: string | null | undefined;
  search?: string | null | undefined;
}>;


export type WorkspacesPreviewQuery = { node:
    | { __typename: 'BackgroundJob' }
    | { __typename: 'BatchChange' }
    | { __typename: 'BatchChangesCredential' }
    | { __typename: 'BatchSpec', id: string, workspaceResolution: { state: BatchSpecWorkspaceResolutionState, failureMessage: string | null, workspaces: { totalCount: number, pageInfo: { hasNextPage: boolean, endCursor: string | null }, stats: { errored: number, completed: number, processing: number, queued: number, ignored: number }, nodes: Array<
            | { __typename: 'HiddenBatchSpecWorkspace', id: string, state: BatchSpecWorkspaceState, ignored: boolean, unsupported: boolean, cachedResultFound: boolean, stepCacheResultCount: number, onlyFetchWorkspace: boolean }
            | { __typename: 'VisibleBatchSpecWorkspace', path: string, searchResultPaths: Array<string>, id: string, state: BatchSpecWorkspaceState, ignored: boolean, unsupported: boolean, cachedResultFound: boolean, stepCacheResultCount: number, onlyFetchWorkspace: boolean, repository: { name: string, url: string }, branch: { displayName: string } }
          > } } | null }
    | { __typename: 'BulkOperation' }
    | { __typename: 'ChangesetEvent' }
    | { __typename: 'CodeIntelligenceConfigurationPolicy' }
    | { __typename: 'CodeownersIngestedFile' }
    | { __typename: 'ExecutorSecret' }
    | { __typename: 'ExecutorSecretAccessLog' }
    | { __typename: 'ExternalChangeset' }
    | { __typename: 'ExternalService' }
    | { __typename: 'HiddenBatchSpecWorkspace' }
    | { __typename: 'HiddenChangesetSpec' }
    | { __typename: 'HiddenExternalChangeset' }
    | { __typename: 'InsightView' }
    | { __typename: 'InsightsDashboard' }
    | { __typename: 'Monitor' }
    | { __typename: 'Notebook' }
    | { __typename: 'Org' }
    | { __typename: 'OrganizationInvitation' }
    | { __typename: 'OutOfBandMigration' }
    | { __typename: 'OutboundRequest' }
    | { __typename: 'OutboundWebhook' }
    | { __typename: 'OutboundWebhookLog' }
    | { __typename: 'PreciseIndex' }
    | { __typename: 'Repository' }
    | { __typename: 'SavedSearch' }
    | { __typename: 'SearchContext' }
    | { __typename: 'SearchJob' }
    | { __typename: 'Team' }
    | { __typename: 'User' }
    | { __typename: 'VisibleBatchSpecWorkspace' }
    | { __typename: 'VisibleChangesetSpec' }
    | { __typename: 'WebhookLog' }
   | null };

export type InferAutoIndexJobsForRepoQueryVariables = Exact<{
  repo: string | number;
  script?: string | null | undefined;
}>;


export type InferAutoIndexJobsForRepoQuery = { inferAutoIndexJobsForRepo: { inferenceOutput: string, jobs: Array<{ root: string, indexer: { name: string, imageName: string | null } | null, steps: { index: { indexerArgs: Array<string> } } }> } };

export type CodeIntelligenceInferenceScriptQueryVariables = Exact<{ [key: string]: never; }>;


export type CodeIntelligenceInferenceScriptQuery = { codeIntelligenceInferenceScript: string };

export type InferenceRepositoryIdQueryVariables = Exact<{
  name: string;
}>;


export type InferenceRepositoryIdQuery = { repository: { id: string } | null };

export type UpdateCodeIntelligenceInferenceScriptMutationVariables = Exact<{
  script: string;
}>;


export type UpdateCodeIntelligenceInferenceScriptMutation = { updateCodeIntelligenceInferenceScript: { alwaysNil: string | null } };

export type CreateCodeIntelPolicyMutationVariables = Exact<{
  repository?: string | number | null | undefined;
  repositoryPatterns?: Array<string> | string | null | undefined;
  name: string;
  type: GitObjectType;
  pattern: string;
  retentionEnabled: boolean;
  retentionDurationHours?: number | null | undefined;
  retainIntermediateCommits: boolean;
  indexingEnabled: boolean;
  indexCommitMaxAgeHours?: number | null | undefined;
  indexIntermediateCommits: boolean;
}>;


export type CreateCodeIntelPolicyMutation = { createCodeIntelligenceConfigurationPolicy: { id: string } };

export type DeleteCodeIntelPolicyMutationVariables = Exact<{
  id: string | number;
}>;


export type DeleteCodeIntelPolicyMutation = { deleteCodeIntelligenceConfigurationPolicy: { alwaysNil: string | null } };

export type CodeIntelPoliciesQueryVariables = Exact<{
  repo?: string | number | null | undefined;
  query?: string | null | undefined;
  first?: number | null | undefined;
  after?: string | null | undefined;
}>;


export type CodeIntelPoliciesQuery = { codeIntelligenceConfigurationPolicies: { totalCount: number | null, pageInfo: { hasNextPage: boolean, endCursor: string | null }, nodes: Array<{ id: string, name: string, type: GitObjectType, pattern: string, protected: boolean, repositoryPatterns: Array<string> | null, retentionEnabled: boolean, retentionDurationHours: number | null, retainIntermediateCommits: boolean, indexingEnabled: boolean, indexCommitMaxAgeHours: number | null, indexIntermediateCommits: boolean, managedBy: string | null, repository: { name: string, url: string } | null }> } };

export type PreviewGitObjectsQueryVariables = Exact<{
  repo: string | number;
  type: GitObjectType;
  pattern: string;
}>;


export type PreviewGitObjectsQuery = { node:
    | { previewGitObjectFilter: { totalCount: number, nodes: Array<{ name: string, rev: string, committedAt: string }> } }
    | Record<PropertyKey, never>
   | null };

export type PreviewRepositoriesQueryVariables = Exact<{
  patterns: Array<string> | string;
}>;


export type PreviewRepositoriesQuery = { previewRepositoryFilter: { totalMatches: number, matchesAllRepos: boolean, nodes: Array<{ name: string, url: string }> } };

export type PreciseIndexRetentionQueryVariables = Exact<{
  id: string | number;
}>;


export type PreciseIndexRetentionQuery = { node:
    | { expired: boolean, retentionPolicyOverview: { nodes: Array<{ matches: boolean, protectingCommits: Array<string> | null, configurationPolicy: { id: string, name: string, type: GitObjectType, pattern: string, retentionDurationHours: number | null } | null }> } }
    | Record<PropertyKey, never>
   | null, dependents: { totalCount: number | null, nodes: Array<{ id: string, inputRoot: string, projectRoot: { url: string, repository: { name: string } } | null }> } };

export type UpdateCodeIntelPolicyMutationVariables = Exact<{
  id: string | number;
  repositoryPatterns?: Array<string> | string | null | undefined;
  name: string;
  type: GitObjectType;
  pattern: string;
  retentionEnabled: boolean;
  retentionDurationHours?: number | null | undefined;
  retainIntermediateCommits: boolean;
  indexingEnabled: boolean;
  indexCommitMaxAgeHours?: number | null | undefined;
  indexIntermediateCommits: boolean;
}>;


export type UpdateCodeIntelPolicyMutation = { updateCodeIntelligenceConfigurationPolicy: { alwaysNil: string | null } };

export type SymbolInfoQueryVariables = Exact<{
  repo: string;
  rev: string;
  path: string;
  line: number;
  character: number;
}>;


export type SymbolInfoQuery = { repository: { commit: { blob: { symbolInfo: { hover: string | null, provenance: CodeGraphDataProvenance, definition: { repo: string, commit: string, path: string, range: { line: number, character: number, length: number } } | null, range: { start: { line: number, character: number }, end: { line: number, character: number } } } | null } | null } | null } | null };

export type UsagesForSymbolQueryVariables = Exact<{
  range: RangeInput;
  filter?: UsagesFilter | null | undefined;
  first?: number | null | undefined;
  after?: string | null | undefined;
}>;


export type UsagesForSymbolQuery = { usagesForSymbol: { nodes: Array<{ provenance: CodeGraphDataProvenance, usageKind: SymbolUsageKind, surroundingContent: string, symbol: { name: string } | null, usageRange: { repository: string, revision: string, path: string, range: { start: { line: number, character: number }, end: { line: number, character: number } } } }>, pageInfo: { hasNextPage: boolean, endCursor: string | null }, coverage: { truncated: boolean, summary: string, searchBasedRepositories: Array<{ repository: string }>, crossVersionRepositories: Array<{ repository: string, version: string }> } | null } };

export type AddExternalServiceMutationVariables = Exact<{
  input: AddExternalServiceInput;
}>;


export type AddExternalServiceMutation = { addExternalService: { id: string, warning: string | null } };

export type CancelExternalServiceSyncMutationVariables = Exact<{
  id: string | number;
}>;


export type CancelExternalServiceSyncMutation = { cancelExternalServiceSync: { alwaysNil: string | null } };

export type CheckExternalServiceConfigQueryVariables = Exact<{
  kind: ExternalServiceKind;
  config: string;
  id?: string | number | null | undefined;
}>;


export type CheckExternalServiceConfigQuery = { checkExternalServiceConfig: { ok: boolean, problem: string | null, account: string | null, scopes: Array<string> | null, warnings: Array<string> } };

export type CheckExternalServiceConnectionQueryVariables = Exact<{
  id: string | number;
}>;


export type CheckExternalServiceConnectionQuery = { node:
    | { __typename: 'BackgroundJob' }
    | { __typename: 'BatchChange' }
    | { __typename: 'BatchChangesCredential' }
    | { __typename: 'BatchSpec' }
    | { __typename: 'BulkOperation' }
    | { __typename: 'ChangesetEvent' }
    | { __typename: 'CodeIntelligenceConfigurationPolicy' }
    | { __typename: 'CodeownersIngestedFile' }
    | { __typename: 'ExecutorSecret' }
    | { __typename: 'ExecutorSecretAccessLog' }
    | { __typename: 'ExternalChangeset' }
    | { __typename: 'ExternalService', checkConnection:
        | { __typename: 'ExternalServiceAvailable', lastCheckedAt: string }
        | { __typename: 'ExternalServiceUnavailable', suspectedReason: string }
        | { __typename: 'ExternalServiceAvailabilityUnknown', implementationNote: string }
       }
    | { __typename: 'HiddenBatchSpecWorkspace' }
    | { __typename: 'HiddenChangesetSpec' }
    | { __typename: 'HiddenExternalChangeset' }
    | { __typename: 'InsightView' }
    | { __typename: 'InsightsDashboard' }
    | { __typename: 'Monitor' }
    | { __typename: 'Notebook' }
    | { __typename: 'Org' }
    | { __typename: 'OrganizationInvitation' }
    | { __typename: 'OutOfBandMigration' }
    | { __typename: 'OutboundRequest' }
    | { __typename: 'OutboundWebhook' }
    | { __typename: 'OutboundWebhookLog' }
    | { __typename: 'PreciseIndex' }
    | { __typename: 'Repository' }
    | { __typename: 'SavedSearch' }
    | { __typename: 'SearchContext' }
    | { __typename: 'SearchJob' }
    | { __typename: 'Team' }
    | { __typename: 'User' }
    | { __typename: 'VisibleBatchSpecWorkspace' }
    | { __typename: 'VisibleChangesetSpec' }
    | { __typename: 'WebhookLog' }
   | null };

export type DeleteExternalServiceMutationVariables = Exact<{
  id: string | number;
}>;


export type DeleteExternalServiceMutation = { deleteExternalService: { alwaysNil: string | null } };

export type ExternalServiceQueryVariables = Exact<{
  id: string | number;
  first?: number | null | undefined;
  after?: string | null | undefined;
}>;


export type ExternalServiceQuery = { node:
    | { __typename: 'BackgroundJob' }
    | { __typename: 'BatchChange' }
    | { __typename: 'BatchChangesCredential' }
    | { __typename: 'BatchSpec' }
    | { __typename: 'BulkOperation' }
    | { __typename: 'ChangesetEvent' }
    | { __typename: 'CodeIntelligenceConfigurationPolicy' }
    | { __typename: 'CodeownersIngestedFile' }
    | { __typename: 'ExecutorSecret' }
    | { __typename: 'ExecutorSecretAccessLog' }
    | { __typename: 'ExternalChangeset' }
    | { __typename: 'ExternalService', id: string, kind: ExternalServiceKind, displayName: string, url: string, repoCount: number, lastSyncAt: string | null, nextSyncAt: string | null, lastSyncError: string | null, updatedAt: string, managedBy: string | null, config: string, createdAt: string, webhookURL: string, webhookSecret: string | null, unrestricted: boolean, hasConnectionCheck: boolean, warning: string | null, rateLimiterState: { burst: number, currentCapacity: number, infinite: boolean, interval: number, lastReplenishment: string, limit: number, pausedUntil: string | null } | null, syncJobs: { totalCount: number, pageInfo: { hasNextPage: boolean, endCursor: string | null }, nodes: Array<{ id: string, state: ExternalServiceSyncJobState, reason: string, queuedAt: string, startedAt: string | null, finishedAt: string | null, failureMessage: string | null, warnings: Array<string>, fullSync: boolean | null, reposSynced: number, reposAdded: number, reposModified: number, reposUnmodified: number, reposDeleted: number, repoSyncErrors: number }> } }
    | { __typename: 'HiddenBatchSpecWorkspace' }
    | { __typename: 'HiddenChangesetSpec' }
    | { __typename: 'HiddenExternalChangeset' }
    | { __typename: 'InsightView' }
    | { __typename: 'InsightsDashboard' }
    | { __typename: 'Monitor' }
    | { __typename: 'Notebook' }
    | { __typename: 'Org' }
    | { __typename: 'OrganizationInvitation' }
    | { __typename: 'OutOfBandMigration' }
    | { __typename: 'OutboundRequest' }
    | { __typename: 'OutboundWebhook' }
    | { __typename: 'OutboundWebhookLog' }
    | { __typename: 'PreciseIndex' }
    | { __typename: 'Repository' }
    | { __typename: 'SavedSearch' }
    | { __typename: 'SearchContext' }
    | { __typename: 'SearchJob' }
    | { __typename: 'Team' }
    | { __typename: 'User' }
    | { __typename: 'VisibleBatchSpecWorkspace' }
    | { __typename: 'VisibleChangesetSpec' }
    | { __typename: 'WebhookLog' }
   | null };

export type ExternalServicesQueryVariables = Exact<{
  first?: number | null | undefined;
  after?: string | null | undefined;
}>;


export type ExternalServicesQuery = { externalServices: { totalCount: number, pageInfo: { hasNextPage: boolean, endCursor: string | null }, nodes: Array<{ id: string, kind: ExternalServiceKind, displayName: string, url: string, repoCount: number, lastSyncAt: string | null, nextSyncAt: string | null, lastSyncError: string | null, updatedAt: string, managedBy: string | null }> }, site: { declaredConfigFiles: Array<{ source: string, kinds: Array<string>, exclusive: boolean }> } };

export type RegenerateExternalServiceWebhookSecretMutationVariables = Exact<{
  id: string | number;
}>;


export type RegenerateExternalServiceWebhookSecretMutation = { regenerateExternalServiceWebhookSecret: { webhookSecret: string, externalService: { id: string, webhookSecret: string | null } } };

export type SyncExternalServiceMutationVariables = Exact<{
  id: string | number;
}>;


export type SyncExternalServiceMutation = { syncExternalService: { alwaysNil: string | null } };

export type UpdateExternalServiceMutationVariables = Exact<{
  input: UpdateExternalServiceInput;
}>;


export type UpdateExternalServiceMutation = { updateExternalService: { id: string, warning: string | null } };

export type ValidateExternalServiceConfigQueryVariables = Exact<{
  kind: ExternalServiceKind;
  config: string;
}>;


export type ValidateExternalServiceConfigQuery = { validateExternalServiceConfig: { errors: Array<string>, warnings: Array<string> } };

export type DeclaredKindsQueryVariables = Exact<{ [key: string]: never; }>;


export type DeclaredKindsQuery = { site: { declaredConfigFiles: Array<{ source: string, kinds: Array<string>, exclusive: boolean }> } };

export type LogEventMutationVariables = Exact<{
  event: string;
  userCookieID: string;
  url: string;
  argument?: string | null | undefined;
}>;


export type LogEventMutation = { logEvent: { alwaysNil: string | null } | null };

export type AddInsightViewToDashboardMutationVariables = Exact<{
  insightViewId: string | number;
  dashboardId: string | number;
}>;


export type AddInsightViewToDashboardMutation = { addInsightViewToDashboard: { dashboard: { id: string } } };

export type CheckRepositoryExistsQueryVariables = Exact<{
  name: string;
}>;


export type CheckRepositoryExistsQuery = { repository: { name: string } | null };

export type CreateInsightsDashboardMutationVariables = Exact<{
  input: CreateInsightsDashboardInput;
}>;


export type CreateInsightsDashboardMutation = { createInsightsDashboard: { dashboard: { id: string, title: string, grants: { users: Array<string>, organizations: Array<string>, global: boolean } } } };

export type CreateLineChartSearchInsightMutationVariables = Exact<{
  input: LineChartSearchInsightInput;
}>;


export type CreateLineChartSearchInsightMutation = { createLineChartSearchInsight: { view: { id: string } } };

export type CreatePieChartSearchInsightMutationVariables = Exact<{
  input: PieChartSearchInsightInput;
}>;


export type CreatePieChartSearchInsightMutation = { createPieChartSearchInsight: { view: { id: string } } };

export type DeleteInsightViewMutationVariables = Exact<{
  id: string | number;
}>;


export type DeleteInsightViewMutation = { deleteInsightView: { alwaysNil: string | null } };

export type DeleteInsightsDashboardMutationVariables = Exact<{
  id: string | number;
}>;


export type DeleteInsightsDashboardMutation = { deleteInsightsDashboard: { alwaysNil: string | null } };

export type InsightPreviewQueryVariables = Exact<{
  input: SearchInsightPreviewInput;
}>;


export type InsightPreviewQuery = { searchInsightPreview: Array<{ label: string, points: Array<{ dateTime: string, value: number, diffQuery: string | null, pointInTimeQuery: string | null }> }> };

export type InsightRepositoriesCountQueryVariables = Exact<{
  query: string;
}>;


export type InsightRepositoriesCountQuery = { previewRepositoriesFromQuery: { numberOfRepositories: number | null } };

export type InsightSuggestionsQueryVariables = Exact<{
  first: number;
  after?: string | null | undefined;
  find?: string | null | undefined;
}>;


export type InsightSuggestionsQuery = { insightViews: { totalCount: number | null, nodes: Array<{ id: string, presentation:
        | { __typename: 'LineChartInsightViewPresentation', title: string }
        | { __typename: 'PieChartInsightViewPresentation', title: string }
      , dataSeriesDefinitions: Array<{ query: string, generatedFromCaptureGroups: boolean, groupBy: GroupByField | null }> } | null>, pageInfo: { hasNextPage: boolean, endCursor: string | null } } };

export type InsightViewQueryVariables = Exact<{
  id: string | number;
  filters?: InsightViewFiltersInput | null | undefined;
  seriesDisplayOptions?: SeriesDisplayOptionsInput | null | undefined;
}>;


export type InsightViewQuery = { insightViews: { nodes: Array<{ id: string, isFrozen: boolean, dashboardReferenceCount: number, seriesCount: number | null, presentation:
        | { __typename: 'LineChartInsightViewPresentation', title: string, seriesPresentation: Array<{ seriesId: string, label: string, color: string }> }
        | { __typename: 'PieChartInsightViewPresentation', title: string, otherThreshold: number }
      , defaultFilters: { includeRepoRegex: string | null, excludeRepoRegex: string | null, searchContexts: Array<string> | null }, appliedFilters: { includeRepoRegex: string | null, excludeRepoRegex: string | null, searchContexts: Array<string> | null }, defaultSeriesDisplayOptions: { limit: number | null, numSamples: number | null, sortOptions: { mode: SeriesSortMode | null, direction: SeriesSortDirection | null } }, appliedSeriesDisplayOptions: { limit: number | null, numSamples: number | null, sortOptions: { mode: SeriesSortMode | null, direction: SeriesSortDirection | null } }, dataSeriesDefinitions: Array<{ __typename: 'SearchInsightDataSeriesDefinition', seriesId: string, query: string, generatedFromCaptureGroups: boolean, isCalculated: boolean, groupBy: GroupByField | null, repositoryDefinition:
          | { __typename: 'InsightRepositoryScope', repositories: Array<string> }
          | { __typename: 'RepositorySearchScope', search: string, allRepositories: boolean }
        , timeScope: { __typename: 'InsightIntervalTimeScope', unit: TimeIntervalStepUnit, value: number } }>, dataSeries: Array<{ seriesId: string, label: string, points: Array<{ dateTime: string, value: number, diffQuery: string | null, pointInTimeQuery: string | null }>, status: { isLoadingData: boolean | null, pendingJobs: number, completedJobs: number, failedJobs: number, backfillQueuedAt: string | null, incompleteDatapoints: Array<
            | { __typename: 'GenericIncompleteDatapointAlert', time: string }
            | { __typename: 'TimeoutDatapointAlert', time: string }
          > } }>, languageStatistics: Array<{ name: string, totalLines: number }> } | null> } };

export type InsightViewIdsQueryVariables = Exact<{
  first: number;
  after?: string | null | undefined;
}>;


export type InsightViewIdsQuery = { insightViews: { totalCount: number | null, nodes: Array<{ id: string } | null>, pageInfo: { hasNextPage: boolean, endCursor: string | null } } };

export type InsightsDashboardQueryVariables = Exact<{
  id: string | number;
}>;


export type InsightsDashboardQuery = { insightsDashboards: { nodes: Array<{ id: string, title: string, grants: { users: Array<string>, organizations: Array<string>, global: boolean }, views: { totalCount: number | null, nodes: Array<{ id: string } | null> } | null }> } };

export type InsightsDashboardOwnersQueryVariables = Exact<{ [key: string]: never; }>;


export type InsightsDashboardOwnersQuery = { currentUser: { id: string, username: string, siteAdmin: boolean, organizations: { nodes: Array<{ id: string, name: string, displayName: string | null }> } } | null };

export type InsightsDashboardsQueryVariables = Exact<{ [key: string]: never; }>;


export type InsightsDashboardsQuery = { insightsDashboards: { nodes: Array<{ id: string, title: string, grants: { users: Array<string>, organizations: Array<string>, global: boolean } }> } };

export type LanguageStatisticsPreviewQueryVariables = Exact<{
  repositories: Array<string> | string;
}>;


export type LanguageStatisticsPreviewQuery = { previewLanguageStatistics: Array<{ name: string, totalLines: number }> };

export type RemoveInsightViewFromDashboardMutationVariables = Exact<{
  insightViewId: string | number;
  dashboardId: string | number;
}>;


export type RemoveInsightViewFromDashboardMutation = { removeInsightViewFromDashboard: { dashboard: { id: string } } };

export type SaveInsightAsNewViewMutationVariables = Exact<{
  input: SaveInsightAsNewViewInput;
}>;


export type SaveInsightAsNewViewMutation = { saveInsightAsNewView: { view: { id: string } } };

export type UpdateInsightsDashboardMutationVariables = Exact<{
  id: string | number;
  input: UpdateInsightsDashboardInput;
}>;


export type UpdateInsightsDashboardMutation = { updateInsightsDashboard: { dashboard: { id: string, title: string, grants: { users: Array<string>, organizations: Array<string>, global: boolean } } } };

export type UpdateLineChartSearchInsightMutationVariables = Exact<{
  id: string | number;
  input: UpdateLineChartSearchInsightInput;
}>;


export type UpdateLineChartSearchInsightMutation = { updateLineChartSearchInsight: { view: { id: string } } };

export type UpdatePieChartSearchInsightMutationVariables = Exact<{
  id: string | number;
  input: UpdatePieChartSearchInsightInput;
}>;


export type UpdatePieChartSearchInsightMutation = { updatePieChartSearchInsight: { view: { id: string } } };

export type ValidateInsightRepoQueryQueryVariables = Exact<{
  query: string;
}>;


export type ValidateInsightRepoQueryQuery = { validateScopedInsightQuery: { isValid: boolean, invalidReason: string | null } };

export type CanShareUnfilteredWatchResultsQueryVariables = Exact<{ [key: string]: never; }>;


export type CanShareUnfilteredWatchResultsQuery = { viewerCanShareUnfilteredWatchResults: boolean };

export type CreateCodeMonitorMutationVariables = Exact<{
  monitor: MonitorInput;
  trigger: MonitorTriggerInput;
  actions: Array<MonitorActionInput> | MonitorActionInput;
}>;


export type CreateCodeMonitorMutation = { createCodeMonitor: { id: string, url: string } };

export type DeleteCodeMonitorMutationVariables = Exact<{
  id: string | number;
}>;


export type DeleteCodeMonitorMutation = { deleteCodeMonitor: { alwaysNil: string | null } };

export type CodeMonitorQueryVariables = Exact<{
  id: string | number;
}>;


export type CodeMonitorQuery = { node:
    | { __typename: 'BackgroundJob' }
    | { __typename: 'BatchChange' }
    | { __typename: 'BatchChangesCredential' }
    | { __typename: 'BatchSpec' }
    | { __typename: 'BulkOperation' }
    | { __typename: 'ChangesetEvent' }
    | { __typename: 'CodeIntelligenceConfigurationPolicy' }
    | { __typename: 'CodeownersIngestedFile' }
    | { __typename: 'ExecutorSecret' }
    | { __typename: 'ExecutorSecretAccessLog' }
    | { __typename: 'ExternalChangeset' }
    | { __typename: 'ExternalService' }
    | { __typename: 'HiddenBatchSpecWorkspace' }
    | { __typename: 'HiddenChangesetSpec' }
    | { __typename: 'HiddenExternalChangeset' }
    | { __typename: 'InsightView' }
    | { __typename: 'InsightsDashboard' }
    | { __typename: 'Monitor', id: string, description: string, enabled: boolean, createdAt: string, url: string, unfilteredResults: boolean, unfilteredAt: string | null, owner:
        | { __typename: 'Org', name: string, id: string }
        | { __typename: 'User', username: string, id: string }
      , runsAs: { username: string } | null, unfilteredBy: { username: string } | null, trigger: { id: string, query: string }, actions: { nodes: Array<
          | { __typename: 'MonitorEmail', id: string, enabled: boolean, includeResults: boolean, priority: MonitorEmailPriority, header: string, recipients: { nodes: Array<
                | { __typename: 'Org', name: string, id: string }
                | { __typename: 'User', username: string, id: string }
              > } }
          | { __typename: 'MonitorWebhook', id: string, enabled: boolean, includeResults: boolean, url: string }
          | { __typename: 'MonitorSlackWebhook', id: string, enabled: boolean, includeResults: boolean, url: string }
          | { __typename: 'MonitorMattermostWebhook', id: string, enabled: boolean, includeResults: boolean, url: string }
          | { __typename: 'MonitorTelegram', id: string, enabled: boolean, includeResults: boolean, botToken: string, chatId: string }
        > } }
    | { __typename: 'Notebook' }
    | { __typename: 'Org' }
    | { __typename: 'OrganizationInvitation' }
    | { __typename: 'OutOfBandMigration' }
    | { __typename: 'OutboundRequest' }
    | { __typename: 'OutboundWebhook' }
    | { __typename: 'OutboundWebhookLog' }
    | { __typename: 'PreciseIndex' }
    | { __typename: 'Repository' }
    | { __typename: 'SavedSearch' }
    | { __typename: 'SearchContext' }
    | { __typename: 'SearchJob' }
    | { __typename: 'Team' }
    | { __typename: 'User' }
    | { __typename: 'VisibleBatchSpecWorkspace' }
    | { __typename: 'VisibleChangesetSpec' }
    | { __typename: 'WebhookLog' }
   | null };

export type CodeMonitorEventsQueryVariables = Exact<{
  id: string | number;
  first: number;
  after?: string | null | undefined;
}>;


export type CodeMonitorEventsQuery = { node:
    | { description: string, trigger: { query: string, events: { totalCount: number, pageInfo: { hasNextPage: boolean, endCursor: string | null }, nodes: Array<{ id: string, status: EventStatus, message: string | null, timestamp: string, resultCount: number, query: string | null, ranges: Array<{ repository: string, revision: string, from: string | null, to: string }>, actions: { nodes: Array<
                | { __typename: 'MonitorEmail', id: string, events: { nodes: Array<{ id: string, status: EventStatus, message: string | null, timestamp: string }> } }
                | { __typename: 'MonitorWebhook', id: string, events: { nodes: Array<{ id: string, status: EventStatus, message: string | null, timestamp: string }> } }
                | { __typename: 'MonitorSlackWebhook', id: string, events: { nodes: Array<{ id: string, status: EventStatus, message: string | null, timestamp: string }> } }
                | { __typename: 'MonitorMattermostWebhook', id: string, events: { nodes: Array<{ id: string, status: EventStatus, message: string | null, timestamp: string }> } }
                | { __typename: 'MonitorTelegram', id: string, events: { nodes: Array<{ id: string, status: EventStatus, message: string | null, timestamp: string }> } }
              > } }> } } }
    | Record<PropertyKey, never>
   | null };

export type ResetCodeMonitorTriggerMutationVariables = Exact<{
  id: string | number;
}>;


export type ResetCodeMonitorTriggerMutation = { resetTriggerQueryTimestamps: { alwaysNil: string | null } };

export type TestCodeMonitorEmailMutationVariables = Exact<{
  namespace: string | number;
  description: string;
  email: MonitorEmailInput;
  action?: string | number | null | undefined;
}>;


export type TestCodeMonitorEmailMutation = { triggerTestEmailAction: { alwaysNil: string | null } };

export type TestCodeMonitorMattermostWebhookMutationVariables = Exact<{
  namespace: string | number;
  description: string;
  mattermostWebhook: MonitorMattermostWebhookInput;
  action?: string | number | null | undefined;
}>;


export type TestCodeMonitorMattermostWebhookMutation = { triggerTestMattermostWebhookAction: { alwaysNil: string | null } };

export type TestCodeMonitorSlackWebhookMutationVariables = Exact<{
  namespace: string | number;
  description: string;
  slackWebhook: MonitorSlackWebhookInput;
  action?: string | number | null | undefined;
}>;


export type TestCodeMonitorSlackWebhookMutation = { triggerTestSlackWebhookAction: { alwaysNil: string | null } };

export type TestCodeMonitorTelegramMutationVariables = Exact<{
  namespace: string | number;
  description: string;
  telegram: MonitorTelegramInput;
  action?: string | number | null | undefined;
}>;


export type TestCodeMonitorTelegramMutation = { triggerTestTelegramAction: { alwaysNil: string | null } };

export type TestCodeMonitorWebhookMutationVariables = Exact<{
  namespace: string | number;
  description: string;
  webhook: MonitorWebhookInput;
  action?: string | number | null | undefined;
}>;


export type TestCodeMonitorWebhookMutation = { triggerTestWebhookAction: { alwaysNil: string | null } };

export type ToggleCodeMonitorMutationVariables = Exact<{
  id: string | number;
  enabled: boolean;
}>;


export type ToggleCodeMonitorMutation = { toggleCodeMonitor: { id: string, enabled: boolean } };

export type UpdateCodeMonitorMutationVariables = Exact<{
  monitor: MonitorEditInput;
  trigger: MonitorEditTriggerInput;
  actions: Array<MonitorEditActionInput> | MonitorEditActionInput;
}>;


export type UpdateCodeMonitorMutation = { updateCodeMonitor: { id: string, url: string } };

export type UserMonitorsQueryVariables = Exact<{
  user: string | number;
  first: number;
  after?: string | null | undefined;
}>;


export type UserMonitorsQuery = { node:
    | { monitors: { totalCount: number, pageInfo: { hasNextPage: boolean, endCursor: string | null }, nodes: Array<{ id: string, description: string, enabled: boolean, createdAt: string, url: string, unfilteredResults: boolean, unfilteredAt: string | null, owner:
            | { __typename: 'Org', name: string, id: string }
            | { __typename: 'User', username: string, id: string }
          , runsAs: { username: string } | null, unfilteredBy: { username: string } | null, trigger: { id: string, query: string }, actions: { nodes: Array<
              | { __typename: 'MonitorEmail', id: string, enabled: boolean, includeResults: boolean, priority: MonitorEmailPriority, header: string, recipients: { nodes: Array<
                    | { __typename: 'Org', name: string, id: string }
                    | { __typename: 'User', username: string, id: string }
                  > } }
              | { __typename: 'MonitorWebhook', id: string, enabled: boolean, includeResults: boolean, url: string }
              | { __typename: 'MonitorSlackWebhook', id: string, enabled: boolean, includeResults: boolean, url: string }
              | { __typename: 'MonitorMattermostWebhook', id: string, enabled: boolean, includeResults: boolean, url: string }
              | { __typename: 'MonitorTelegram', id: string, enabled: boolean, includeResults: boolean, botToken: string, chatId: string }
            > } }> } }
    | Record<PropertyKey, never>
   | null };

export type CreateNotebookMutationVariables = Exact<{
  notebook: NotebookInput;
}>;


export type CreateNotebookMutation = { createNotebook: { id: string } };

export type CreateNotebookStarMutationVariables = Exact<{
  id: string | number;
}>;


export type CreateNotebookStarMutation = { createNotebookStar: { createdAt: string } };

export type DeleteNotebookMutationVariables = Exact<{
  id: string | number;
}>;


export type DeleteNotebookMutation = { deleteNotebook: { alwaysNil: string | null } };

export type DeleteNotebookStarMutationVariables = Exact<{
  id: string | number;
}>;


export type DeleteNotebookStarMutation = { deleteNotebookStar: { alwaysNil: string | null } };

export type NotebookQueryVariables = Exact<{
  id: string | number;
}>;


export type NotebookQuery = { notebook: { id: string, title: string, public: boolean, updatedAt: string, createdAt: string, viewerHasStarred: boolean, viewerCanManage: boolean, patternType: SearchPatternType, creator: { username: string } | null, namespace:
      | { __typename: 'Org', name: string, id: string, url: string }
      | { __typename: 'User', username: string, id: string, url: string }
     | null, stars: { totalCount: number }, updater: { username: string } | null, blocks: Array<
      | { __typename: 'MarkdownBlock', id: string, markdownInput: string }
      | { __typename: 'QueryBlock', id: string, queryInput: string }
      | { __typename: 'FileBlock', id: string, fileInput: { repositoryName: string, filePath: string, revision: string | null, lineRange: { startLine: number, endLine: number } | null } }
      | { __typename: 'SymbolBlock', id: string, symbolInput: { repositoryName: string, filePath: string, revision: string | null, lineContext: number, symbolName: string, symbolContainerName: string, symbolKind: SymbolKind } }
    > } | null };

export type NotebooksQueryVariables = Exact<{
  first: number;
  after?: string | null | undefined;
  query?: string | null | undefined;
  creatorUserID?: string | number | null | undefined;
  starredByUserID?: string | number | null | undefined;
  namespace?: string | number | null | undefined;
  orderBy?: NotebooksOrderBy | null | undefined;
  descending?: boolean | null | undefined;
}>;


export type NotebooksQuery = { notebooks: { totalCount: number, pageInfo: { hasNextPage: boolean, endCursor: string | null }, nodes: Array<{ id: string, title: string, public: boolean, updatedAt: string, createdAt: string, viewerHasStarred: boolean, creator: { username: string } | null, namespace:
        | { __typename: 'Org', name: string, id: string, url: string }
        | { __typename: 'User', username: string, id: string, url: string }
       | null, stars: { totalCount: number } }> } };

export type UpdateNotebookMutationVariables = Exact<{
  id: string | number;
  notebook: NotebookInput;
}>;


export type UpdateNotebookMutation = { updateNotebook: { id: string, title: string, public: boolean, updatedAt: string, createdAt: string, viewerHasStarred: boolean, viewerCanManage: boolean, patternType: SearchPatternType, creator: { username: string } | null, namespace:
      | { __typename: 'Org', name: string, id: string, url: string }
      | { __typename: 'User', username: string, id: string, url: string }
     | null, stars: { totalCount: number }, updater: { username: string } | null, blocks: Array<
      | { __typename: 'MarkdownBlock', id: string, markdownInput: string }
      | { __typename: 'QueryBlock', id: string, queryInput: string }
      | { __typename: 'FileBlock', id: string, fileInput: { repositoryName: string, filePath: string, revision: string | null, lineRange: { startLine: number, endLine: number } | null } }
      | { __typename: 'SymbolBlock', id: string, symbolInput: { repositoryName: string, filePath: string, revision: string | null, lineContext: number, symbolName: string, symbolContainerName: string, symbolKind: SymbolKind } }
    > } };

export type AllOAuthGrantsQueryVariables = Exact<{ [key: string]: never; }>;


export type AllOAuthGrantsQuery = { oauthGrants: Array<{ id: string, clientID: string, clientName: string, clientURI: string | null, redirectHosts: Array<string>, scopes: Array<string>, createdAt: string, lastUsedAt: string | null, user: { id: string, username: string } }> };

export type CurrentUserOAuthGrantsQueryVariables = Exact<{ [key: string]: never; }>;


export type CurrentUserOAuthGrantsQuery = { currentUser: { id: string, oauthGrants: Array<{ id: string, clientID: string, clientName: string, clientURI: string | null, redirectHosts: Array<string>, scopes: Array<string>, createdAt: string, lastUsedAt: string | null, user: { id: string, username: string } }> } | null };

export type RevokeOAuthGrantMutationVariables = Exact<{
  id: string | number;
}>;


export type RevokeOAuthGrantMutation = { revokeOAuthGrant: { alwaysNil: string | null } };

export type AddTeamMembersMutationVariables = Exact<{
  team: string | number;
  members: Array<TeamMemberInput> | TeamMemberInput;
}>;


export type AddTeamMembersMutation = { addTeamMembers: { id: string } };

export type AddUserToOrganizationMutationVariables = Exact<{
  organization: string | number;
  username: string;
}>;


export type AddUserToOrganizationMutation = { addUserToOrganization: { alwaysNil: string | null } };

export type CreateOrganizationMutationVariables = Exact<{
  name: string;
  displayName?: string | null | undefined;
}>;


export type CreateOrganizationMutation = { createOrganization: { id: string, name: string, url: string } };

export type CreateTeamMutationVariables = Exact<{
  name: string;
  displayName?: string | null | undefined;
  parentTeamName?: string | null | undefined;
}>;


export type CreateTeamMutation = { createTeam: { id: string, name: string, url: string } };

export type DefaultSettingsQueryVariables = Exact<{ [key: string]: never; }>;


export type DefaultSettingsQuery = { site: { defaultSettings: string } };

export type DeleteOrganizationMutationVariables = Exact<{
  organization: string | number;
}>;


export type DeleteOrganizationMutation = { deleteOrganization: { alwaysNil: string | null } | null };

export type DeleteTeamMutationVariables = Exact<{
  id: string | number;
}>;


export type DeleteTeamMutation = { deleteTeam: { alwaysNil: string | null } };

export type GlobalSettingsQueryVariables = Exact<{ [key: string]: never; }>;


export type GlobalSettingsQuery = { site: { id: string, viewerCanAdminister: boolean, latestSettings: { id: number, contents: string, createdAt: string, readOnly: boolean, author: { username: string } | null } | null, declaredConfigFiles: Array<{ source: string, path: string }> } };

export type InvitationByTokenQueryVariables = Exact<{
  token: string;
}>;


export type InvitationByTokenQuery = { invitationByToken: { id: string, recipientEmail: string | null, isVerifiedEmail: boolean | null, expiresAt: string | null, respondedAt: string | null, revokedAt: string | null, organization: { name: string, displayName: string | null, url: string }, sender: { username: string } | null } };

export type InviteUserToOrganizationMutationVariables = Exact<{
  organization: string | number;
  username?: string | null | undefined;
  email?: string | null | undefined;
}>;


export type InviteUserToOrganizationMutation = { inviteUserToOrganization: { sentInvitationEmail: boolean, invitationURL: string } };

export type OrganizationQueryVariables = Exact<{
  name: string;
}>;


export type OrganizationQuery = { organization: { id: string, name: string, displayName: string | null, url: string, createdAt: string, viewerIsMember: boolean, viewerCanAdminister: boolean, viewerPendingInvitation: { id: string, sender: { username: string } | null } | null } | null };

export type OrganizationMembersQueryVariables = Exact<{
  name: string;
  first?: number | null | undefined;
  after?: string | null | undefined;
  query?: string | null | undefined;
}>;


export type OrganizationMembersQuery = { organization: { id: string, members: { totalCount: number, pageInfo: { hasNextPage: boolean, endCursor: string | null }, nodes: Array<{ id: string, username: string, displayName: string | null, avatarURL: string | null }> } } | null };

export type OrganizationSettingsQueryVariables = Exact<{
  name: string;
}>;


export type OrganizationSettingsQuery = { organization: { id: string, displayName: string | null, latestSettings: { id: number, contents: string, createdAt: string, author: { username: string } | null } | null, settingsCascade: { final: string } } | null };

export type OrganizationsQueryVariables = Exact<{
  first?: number | null | undefined;
  query?: string | null | undefined;
}>;


export type OrganizationsQuery = { organizations: { totalCount: number, nodes: Array<{ id: string, name: string, displayName: string | null, url: string, createdAt: string }> } };

export type OverwriteSettingsMutationVariables = Exact<{
  subject: string | number;
  lastID?: number | null | undefined;
  contents: string;
}>;


export type OverwriteSettingsMutation = { settingsMutation: { overwriteSettings: { empty: { alwaysNil: string | null } | null } | null } | null };

export type PendingInvitationsQueryVariables = Exact<{
  org: string | number;
}>;


export type PendingInvitationsQuery = { pendingInvitations: Array<{ id: string, recipientEmail: string | null, createdAt: string, notifiedAt: string | null, expiresAt: string | null, recipient: { username: string } | null, sender: { username: string } | null }> };

export type RemoveTeamMembersMutationVariables = Exact<{
  team: string | number;
  members: Array<TeamMemberInput> | TeamMemberInput;
}>;


export type RemoveTeamMembersMutation = { removeTeamMembers: { id: string } };

export type RemoveUserFromOrganizationMutationVariables = Exact<{
  user: string | number;
  organization: string | number;
}>;


export type RemoveUserFromOrganizationMutation = { removeUserFromOrganization: { alwaysNil: string | null } | null };

export type ResendOrganizationInvitationMutationVariables = Exact<{
  id: string | number;
}>;


export type ResendOrganizationInvitationMutation = { resendOrganizationInvitationNotification: { alwaysNil: string | null } };

export type RespondToOrganizationInvitationMutationVariables = Exact<{
  id: string | number;
  response: OrganizationInvitationResponseType;
}>;


export type RespondToOrganizationInvitationMutation = { respondToOrganizationInvitation: { alwaysNil: string | null } };

export type RevokeOrganizationInvitationMutationVariables = Exact<{
  id: string | number;
}>;


export type RevokeOrganizationInvitationMutation = { revokeOrganizationInvitation: { alwaysNil: string | null } };

export type SyncTeamsFromGitLabGroupsMutationVariables = Exact<{ [key: string]: never; }>;


export type SyncTeamsFromGitLabGroupsMutation = { syncTeamsFromGitLabGroups: { created: number, updated: number, deleted: number, skipped: number, members: number, warnings: Array<string> } };

export type TeamQueryVariables = Exact<{
  name: string;
}>;


export type TeamQuery = { team: { id: string, name: string, displayName: string | null, url: string, readonly: boolean, syncedFromCodeHost: boolean, viewerCanAdminister: boolean, creator: { username: string } | null, parentTeam: { id: string, name: string, displayName: string | null, url: string } | null, childTeams: { totalCount: number, nodes: Array<{ id: string, name: string, displayName: string | null, url: string, readonly: boolean, syncedFromCodeHost: boolean, members: { totalCount: number }, childTeams: { totalCount: number } }> }, members: { totalCount: number, nodes: Array<{ id: string, username: string, displayName: string | null, avatarURL: string | null }> } } | null };

export type TeamsQueryVariables = Exact<{
  first?: number | null | undefined;
  after?: string | null | undefined;
  search?: string | null | undefined;
  includeChildTeams?: boolean | null | undefined;
}>;


export type TeamsQuery = { teams: { totalCount: number, pageInfo: { hasNextPage: boolean, endCursor: string | null }, nodes: Array<{ id: string, name: string, displayName: string | null, url: string, readonly: boolean, syncedFromCodeHost: boolean, members: { totalCount: number }, childTeams: { totalCount: number } }> } };

export type UpdateOrganizationMutationVariables = Exact<{
  id: string | number;
  displayName?: string | null | undefined;
}>;


export type UpdateOrganizationMutation = { updateOrganization: { id: string, displayName: string | null } };

export type UpdateTeamMutationVariables = Exact<{
  id: string | number;
  displayName?: string | null | undefined;
  parentTeamName?: string | null | undefined;
  makeRoot?: boolean | null | undefined;
}>;


export type UpdateTeamMutation = { updateTeam: { id: string } };

export type ViewerOrganizationsQueryVariables = Exact<{ [key: string]: never; }>;


export type ViewerOrganizationsQuery = { currentUser: { id: string, organizations: { totalCount: number, nodes: Array<{ id: string, name: string, displayName: string | null, url: string, createdAt: string }> } } | null, viewerOrganizationInvitations: Array<{ id: string, recipientEmail: string | null, isVerifiedEmail: boolean | null, expiresAt: string | null, organization: { name: string, displayName: string | null }, sender: { username: string } | null }> };

export type AddCodeownersFileMutationVariables = Exact<{
  input: CodeownersFileInput;
}>;


export type AddCodeownersFileMutation = { addCodeownersFile: { id: string } };

export type AssignOwnerMutationVariables = Exact<{
  input: AssignOwnerOrTeamInput;
}>;


export type AssignOwnerMutation = { assignOwner: { alwaysNil: string | null } };

export type AssignTeamMutationVariables = Exact<{
  input: AssignOwnerOrTeamInput;
}>;


export type AssignTeamMutation = { assignTeam: { alwaysNil: string | null } };

export type BlobOwnershipQueryVariables = Exact<{
  repo: string;
  rev: string;
  path: string;
}>;


export type BlobOwnershipQuery = { repository: { id: string, commit: { blob: { ownership: { totalCount: number, totalOwners: number, nodes: Array<{ owner:
              | { __typename: 'Person', name: string, email: string, displayName: string, avatarURL: string | null, user: { id: string, username: string, displayName: string | null, url: string } | null }
              | { __typename: 'Team', id: string, name: string, url: string, external: boolean, teamDisplayName: string | null }
            , reasons: Array<
              | { __typename: 'CodeownersFileEntry', ruleLineMatch: number, section: string, pattern: string, codeownersFile:
                  | { __typename: 'GitBlob', path: string, url: string }
                  | { __typename: 'VirtualFile', path: string, url: string }
                 }
              | { __typename: 'RecentContributorOwnershipSignal', contributionCount: number }
              | { __typename: 'RecentViewOwnershipSignal', viewCount: number }
              | { __typename: 'AssignedOwner', isDirectMatch: boolean }
            > }> } } | null } | null } | null, currentUser: { id: string, permissions: { nodes: Array<{ namespace: string, action: string }> } } | null };

export type CodeownersFilesQueryVariables = Exact<{
  first?: number | null | undefined;
  after?: number | null | undefined;
}>;


export type CodeownersFilesQuery = { codeownersIngestedFiles: { totalCount: number, pageInfo: { hasNextPage: boolean, endCursor: string | null }, nodes: Array<{ id: string, contents: string, createdAt: string, updatedAt: string, repository: { id: string, name: string, url: string } | null }> } };

export type DeleteCodeownersFilesMutationVariables = Exact<{
  repositories: Array<DeleteCodeownersFilesInput> | DeleteCodeownersFilesInput;
}>;


export type DeleteCodeownersFilesMutation = { deleteCodeownersFiles: { alwaysNil: string | null } };

export type OwnSignalConfigurationsQueryVariables = Exact<{ [key: string]: never; }>;


export type OwnSignalConfigurationsQuery = { ownSignalConfigurations: Array<{ name: string, description: string, isEnabled: boolean, excludedRepoPatterns: Array<string> }> };

export type OwnershipAnalyticsQueryVariables = Exact<{
  first?: number | null | undefined;
  after?: string | null | undefined;
}>;


export type OwnershipAnalyticsQuery = { instanceOwnershipStats: { totalFiles: number, totalCodeownedFiles: number, totalOwnedFiles: number, totalAssignedOwnershipFiles: number, updatedAt: string | null }, ownershipStatsByRepository: { totalCount: number, pageInfo: { hasNextPage: boolean, endCursor: string | null }, nodes: Array<{ name: string, repository: { id: string, url: string } | null, stats: { totalFiles: number, totalCodeownedFiles: number, totalOwnedFiles: number, totalAssignedOwnershipFiles: number, updatedAt: string | null } }> }, ownSignalConfigurations: Array<{ name: string, isEnabled: boolean }> };

export type RemoveAssignedOwnerMutationVariables = Exact<{
  input: AssignOwnerOrTeamInput;
}>;


export type RemoveAssignedOwnerMutation = { removeAssignedOwner: { alwaysNil: string | null } };

export type RemoveAssignedTeamMutationVariables = Exact<{
  input: AssignOwnerOrTeamInput;
}>;


export type RemoveAssignedTeamMutation = { removeAssignedTeam: { alwaysNil: string | null } };

export type OwnTeamByNameQueryVariables = Exact<{
  name: string;
}>;


export type OwnTeamByNameQuery = { team: { id: string } | null };

export type TreeOwnershipQueryVariables = Exact<{
  repo: string;
  rev: string;
  path: string;
}>;


export type TreeOwnershipQuery = { repository: { id: string, commit: { tree: { ownership: { totalCount: number, totalOwners: number, nodes: Array<{ owner:
              | { __typename: 'Person', name: string, email: string, displayName: string, avatarURL: string | null, user: { id: string, username: string, displayName: string | null, url: string } | null }
              | { __typename: 'Team', id: string, name: string, url: string, external: boolean, teamDisplayName: string | null }
            , reasons: Array<
              | { __typename: 'CodeownersFileEntry', ruleLineMatch: number, section: string, pattern: string, codeownersFile:
                  | { __typename: 'GitBlob', path: string, url: string }
                  | { __typename: 'VirtualFile', path: string, url: string }
                 }
              | { __typename: 'RecentContributorOwnershipSignal', contributionCount: number }
              | { __typename: 'RecentViewOwnershipSignal', viewCount: number }
              | { __typename: 'AssignedOwner', isDirectMatch: boolean }
            > }> }, ownershipStats: { totalFiles: number, totalCodeownedFiles: number, totalOwnedFiles: number, totalAssignedOwnershipFiles: number, updatedAt: string | null } } | null } | null } | null, currentUser: { id: string, permissions: { nodes: Array<{ namespace: string, action: string }> } } | null };

export type UpdateCodeownersFileMutationVariables = Exact<{
  input: CodeownersFileInput;
}>;


export type UpdateCodeownersFileMutation = { updateCodeownersFile: { id: string } };

export type UpdateOwnSignalConfigurationsMutationVariables = Exact<{
  input: UpdateSignalConfigurationsInput;
}>;


export type UpdateOwnSignalConfigurationsMutation = { updateOwnSignalConfigurations: Array<{ name: string, description: string, isEnabled: boolean, excludedRepoPatterns: Array<string> }> };

export type OwnUserByNameQueryVariables = Exact<{
  username: string;
}>;


export type OwnUserByNameQuery = { user: { id: string } | null };

export type AccessRequestsQueryVariables = Exact<{
  first?: number | null | undefined;
}>;


export type AccessRequestsQuery = { accessRequests: { totalCount: number, nodes: Array<{ id: string, name: string, email: string, additionalInfo: string, createdAt: string }> } };

export type BumpDerivativeGraphKeyMutationVariables = Exact<{ [key: string]: never; }>;


export type BumpDerivativeGraphKeyMutation = { bumpDerivativeGraphKey: { alwaysNil: string | null } };

export type CancelPermissionsSyncJobMutationVariables = Exact<{
  job: string | number;
}>;


export type CancelPermissionsSyncJobMutation = { cancelPermissionsSyncJob: { alwaysNil: string | null } };

export type CreateAccessTokenMutationVariables = Exact<{
  user: string | number;
  scopes: Array<string> | string;
  note: string;
}>;


export type CreateAccessTokenMutation = { createAccessToken: { id: string, token: string } };

export type CreateExecutorSecretMutationVariables = Exact<{
  scope: ExecutorSecretScope;
  key: string;
  value: string;
  namespace?: string | number | null | undefined;
}>;


export type CreateExecutorSecretMutation = { createExecutorSecret: { id: string } };

export type CreateRoleMutationVariables = Exact<{
  name: string;
  permissions: Array<string | number> | string | number;
}>;


export type CreateRoleMutation = { createRole: { id: string } };

export type CurrentUserAccessTokensQueryVariables = Exact<{ [key: string]: never; }>;


export type CurrentUserAccessTokensQuery = { currentUser: { id: string, username: string, siteAdmin: boolean, accessTokens: { nodes: Array<{ id: string, note: string, scopes: Array<string>, createdAt: string, lastUsedAt: string | null, expiresAt: string | null }> } } | null };

export type DeleteAccessTokenMutationVariables = Exact<{
  byID: string | number;
}>;


export type DeleteAccessTokenMutation = { deleteAccessToken: { alwaysNil: string | null } };

export type DeleteExecutorSecretMutationVariables = Exact<{
  scope: ExecutorSecretScope;
  id: string | number;
}>;


export type DeleteExecutorSecretMutation = { deleteExecutorSecret: { alwaysNil: string | null } | null };

export type DeleteRoleMutationVariables = Exact<{
  role: string | number;
}>;


export type DeleteRoleMutation = { deleteRole: { alwaysNil: string | null } };

export type ExecutorSecretAccessLogsQueryVariables = Exact<{
  id: string | number;
  first?: number | null | undefined;
}>;


export type ExecutorSecretAccessLogsQuery = { node:
    | { accessLogs: { totalCount: number, nodes: Array<{ id: string, machineUser: string, createdAt: string, user: { username: string } | null }> } }
    | Record<PropertyKey, never>
   | null };

export type ExecutorsQueryVariables = Exact<{
  query?: string | null | undefined;
  active?: boolean | null | undefined;
  first?: number | null | undefined;
}>;


export type ExecutorsQuery = { areExecutorsConfigured: boolean, executors: { totalCount: number, nodes: Array<{ id: string, hostname: string, queueNames: Array<string>, active: boolean, os: string, architecture: string, executorVersion: string, runtime: string, runtimeVersion: string, gitVersion: string, compatibility: ExecutorCompatibility | null, firstSeenAt: string, lastSeenAt: string, activeJobs: Array<{ queue: string, id: number }> }> } };

export type GlobalExecutorSecretsQueryVariables = Exact<{
  scope: ExecutorSecretScope;
  first?: number | null | undefined;
}>;


export type GlobalExecutorSecretsQuery = { executorSecrets: { totalCount: number, nodes: Array<{ id: string, key: string, scope: ExecutorSecretScope, overwritesGlobalSecret: boolean, createdAt: string, updatedAt: string, managedBy: string | null, namespace:
        | { name: string, id: string }
        | { username: string, id: string }
       | null, creator: { username: string } | null, accessLogs: { totalCount: number } }> }, site: { declaredConfigFiles: Array<{ source: string, kinds: Array<string>, exclusive: boolean }> } };

export type OrganizeSiteConfigurationQueryVariables = Exact<{
  input: string;
}>;


export type OrganizeSiteConfigurationQuery = { site: { organizeConfiguration: string } };

export type PermissionsSubjectsQueryVariables = Exact<{
  username: string;
  repo: string;
}>;


export type PermissionsSubjectsQuery = { user: { id: string, username: string, permissionsInfo: { syncedAt: string | null } } | null, repository: { id: string, name: string } | null };

export type PermissionsSyncJobsQueryVariables = Exact<{
  first?: number | null | undefined;
  after?: string | null | undefined;
  state?: PermissionsSyncJobState | null | undefined;
}>;


export type PermissionsSyncJobsQuery = { permissionsSyncJobs: { totalCount: number, nodes: Array<{ id: string, state: PermissionsSyncJobState, reason: string, queuedAt: string, finishedAt: string | null, ranForMs: number | null, failureMessage: string | null, permissionsAdded: number, permissionsRemoved: number, permissionsFound: number, codeHostStates: Array<{ providerID: string, providerType: string, status: string, message: string }>, subject:
        | { __typename: 'Repository', name: string }
        | { __typename: 'User', username: string }
       | null }>, pageInfo: { hasNextPage: boolean, endCursor: string | null } } };

export type PreciseIndexCoverageQueryVariables = Exact<{
  staleAfterDays: number;
  state?: PreciseIndexCoverageState | null | undefined;
  query?: string | null | undefined;
  first: number;
  after?: string | null | undefined;
}>;


export type PreciseIndexCoverageQuery = { preciseIndexCoverage: { staleAfterDays: number, autoIndexingEnabled: boolean, counts: { upToDate: number, stale: number, failing: number, none: number, notTargeted: number, total: number }, repositories: { totalCount: number, pageInfo: { hasNextPage: boolean, endCursor: string | null }, nodes: Array<{ repositoryID: string, repositoryName: string, codeGraphURL: string, state: PreciseIndexCoverageState, headCommit: string | null, indexedCommit: string | null, indexedAt: string | null, indexedAtHead: boolean, failureMessage: string | null, failedAt: string | null, failureSource: string | null }> } } };

export type RankingSummaryQueryVariables = Exact<{ [key: string]: never; }>;


export type RankingSummaryQuery = { rankingSummary: { derivativeGraphKey: string | null, enabled: boolean, numExportedIndexes: number, numRankedRepositories: number, numRankedPaths: number, rankingSummary: Array<{ graphKey: string, visibleToSearch: boolean, startedAt: string, completedAt: string | null, failureMessage: string | null }> } };

export type RbacQueryVariables = Exact<{ [key: string]: never; }>;


export type RbacQuery = { roles: { nodes: Array<{ id: string, name: string, system: boolean, createdAt: string, managedBy: string | null, permissions: { nodes: Array<{ id: string, namespace: string, action: string, displayName: string }> } }> }, permissions: { nodes: Array<{ id: string, namespace: string, action: string, displayName: string }> }, site: { declaredConfigFiles: Array<{ source: string, kinds: Array<string>, exclusive: boolean }> } };

export type RepositoryAccessExplanationQueryVariables = Exact<{
  user: string | number;
  repository: string | number;
}>;


export type RepositoryAccessExplanationQuery = { repositoryAccessExplanation: { canRead: boolean, summary: string, userSyncedAt: string | null, repositorySyncedAt: string | null, reasons: Array<{ kind: RepositoryAccessReasonKind, detail: string, grantedAt: string | null }> } | null };

export type ScheduleRepositoryPermissionsSyncMutationVariables = Exact<{
  repository: string | number;
}>;


export type ScheduleRepositoryPermissionsSyncMutation = { scheduleRepositoryPermissionsSync: { alwaysNil: string | null } };

export type ScheduleUserPermissionsSyncMutationVariables = Exact<{
  user: string | number;
}>;


export type ScheduleUserPermissionsSyncMutation = { scheduleUserPermissionsSync: { alwaysNil: string | null } };

export type SetAccessRequestStatusMutationVariables = Exact<{
  id: string | number;
  status: AccessRequestStatus;
}>;


export type SetAccessRequestStatusMutation = { setAccessRequestStatus: { resetUrl: string | null } };

export type SetPermissionsMutationVariables = Exact<{
  role: string | number;
  permissions: Array<string | number> | string | number;
}>;


export type SetPermissionsMutation = { setPermissions: { alwaysNil: string | null } };

export type SetRolesMutationVariables = Exact<{
  user: string | number;
  roles: Array<string | number> | string | number;
}>;


export type SetRolesMutation = { setRoles: { alwaysNil: string | null } };

export type SetUserIsSiteAdminMutationVariables = Exact<{
  userID: string | number;
  siteAdmin: boolean;
}>;


export type SetUserIsSiteAdminMutation = { setUserIsSiteAdmin: { alwaysNil: string | null } };

export type SiteConfigurationQueryVariables = Exact<{ [key: string]: never; }>;


export type SiteConfigurationQuery = { site: { id: string, configuration: { id: number, effectiveContents: string, validationMessages: Array<string>, readOnly: boolean }, declaredConfigFiles: Array<{ source: string, path: string, kinds: Array<string>, exclusive: boolean, appliedAt: string | null, error: string | null, errorAt: string | null, checkedAt: string }> } };

export type UpdateExecutorSecretMutationVariables = Exact<{
  scope: ExecutorSecretScope;
  id: string | number;
  value: string;
}>;


export type UpdateExecutorSecretMutation = { updateExecutorSecret: { id: string } };

export type UpdateSiteConfigurationMutationVariables = Exact<{
  lastID: number;
  input: string;
}>;


export type UpdateSiteConfigurationMutation = { updateSiteConfiguration: boolean };

export type UserExecutorSecretsQueryVariables = Exact<{
  scope: ExecutorSecretScope;
  first?: number | null | undefined;
}>;


export type UserExecutorSecretsQuery = { currentUser: { id: string, executorSecrets: { totalCount: number, nodes: Array<{ id: string, key: string, scope: ExecutorSecretScope, overwritesGlobalSecret: boolean, createdAt: string, updatedAt: string, managedBy: string | null, namespace:
          | { name: string, id: string }
          | { username: string, id: string }
         | null, creator: { username: string } | null, accessLogs: { totalCount: number } }> } } | null };

export type UsersQueryVariables = Exact<{
  first?: number | null | undefined;
  query?: string | null | undefined;
  includeDeactivated?: boolean | null | undefined;
}>;


export type UsersQuery = { users: { totalCount: number, nodes: Array<{ id: string, username: string, displayName: string | null, siteAdmin: boolean, scimControlled: boolean, createdAt: string, deactivatedAt: string | null, roles: { nodes: Array<{ id: string, name: string, system: boolean }> } }> } };

export type DeletePreciseIndexMutationVariables = Exact<{
  id: string | number;
}>;


export type DeletePreciseIndexMutation = { deletePreciseIndex: { alwaysNil: string | null } };

export type IndexConfigurationQueryVariables = Exact<{
  repo: string | number;
}>;


export type IndexConfigurationQuery = { node:
    | { indexConfiguration: { configuration: string | null, inferredConfiguration: { configuration: string, limitError: string | null, parsedConfiguration: Array<{ root: string, indexer: { name: string } | null }> | null } } | null }
    | Record<PropertyKey, never>
   | null };

export type PreciseIndexesQueryVariables = Exact<{
  repo: string | number;
  query?: string | null | undefined;
  states?: Array<PreciseIndexState> | PreciseIndexState | null | undefined;
  indexerKey?: string | null | undefined;
  first?: number | null | undefined;
  after?: string | null | undefined;
}>;


export type PreciseIndexesQuery = { indexerKeys: Array<string>, preciseIndexes: { totalCount: number | null, pageInfo: { hasNextPage: boolean, endCursor: string | null }, nodes: Array<{ id: string, inputCommit: string, inputRoot: string, inputIndexer: string, state: PreciseIndexState, uploadedAt: string | null, processingStartedAt: string | null, processingFinishedAt: string | null, failure: string | null, placeInQueue: number | null, uploadSize: number | null, numDocuments: number | null, numOccurrences: number | null, expired: boolean, queuedAt: string | null, indexingStartedAt: string | null, indexingFinishedAt: string | null, shouldReindex: boolean, gitlabCiJob: { jobId: string, ref: string, username: string } | null, steps: { setup: Array<{ key: string, command: Array<string>, startTime: string | null, exitCode: number | null, out: string, durationMilliseconds: number | null }>, preIndex: Array<{ root: string, image: string, commands: Array<string>, logEntry: { key: string, command: Array<string>, startTime: string | null, exitCode: number | null, out: string, durationMilliseconds: number | null } | null }>, index: { commands: Array<string>, indexerArgs: Array<string>, outfile: string | null, requestedEnvVars: Array<string> | null, logEntry: { key: string, command: Array<string>, startTime: string | null, exitCode: number | null, out: string, durationMilliseconds: number | null } | null }, upload: { key: string, command: Array<string>, startTime: string | null, exitCode: number | null, out: string, durationMilliseconds: number | null } | null, teardown: Array<{ key: string, command: Array<string>, startTime: string | null, exitCode: number | null, out: string, durationMilliseconds: number | null }> } | null, indexer: { key: string, name: string, url: string } | null, projectRoot: { url: string, commit: { abbreviatedOID: string, url: string } } | null }> } };

export type QueueAutoIndexJobsForRepoMutationVariables = Exact<{
  repo: string | number;
  rev?: string | null | undefined;
  configuration?: string | null | undefined;
}>;


export type QueueAutoIndexJobsForRepoMutation = { queueAutoIndexJobsForRepo: Array<{ id: string, inputRoot: string }> };

export type ReindexPreciseIndexMutationVariables = Exact<{
  id: string | number;
}>;


export type ReindexPreciseIndexMutation = { reindexPreciseIndex: { alwaysNil: string | null } };

export type UpdateRepositoryIndexConfigurationMutationVariables = Exact<{
  repo: string | number;
  configuration: string;
}>;


export type UpdateRepositoryIndexConfigurationMutation = { updateRepositoryIndexConfiguration: { alwaysNil: string | null } };

export type ReadmeBlobQueryVariables = Exact<{
  repo: string;
  rev: string;
  path: string;
}>;


export type ReadmeBlobQuery = { repository: { commit: { blob: { path: string, name: string, url: string, binary: boolean, contentTruncated: boolean, content: string } | null } | null } | null };

export type AddRepoKeyValuePairMutationVariables = Exact<{
  repo: string | number;
  key: string;
  value?: string | null | undefined;
}>;


export type AddRepoKeyValuePairMutation = { addRepoKeyValuePair: { alwaysNil: string | null } };

export type DeleteRepoKeyValuePairMutationVariables = Exact<{
  repo: string | number;
  key: string;
}>;


export type DeleteRepoKeyValuePairMutation = { deleteRepoKeyValuePair: { alwaysNil: string | null } };

export type RepositoryMetadataQueryVariables = Exact<{
  name: string;
}>;


export type RepositoryMetadataQuery = { repository: { id: string, keyValuePairs: Array<{ key: string, value: string | null }> } | null, currentUser: { id: string, permissions: { nodes: Array<{ namespace: string, action: string }> } } | null };

export type UpdateRepoKeyValuePairMutationVariables = Exact<{
  repo: string | number;
  key: string;
  value?: string | null | undefined;
}>;


export type UpdateRepoKeyValuePairMutation = { updateRepoKeyValuePair: { alwaysNil: string | null } };

export type ViewerPermissionsQueryVariables = Exact<{ [key: string]: never; }>;


export type ViewerPermissionsQuery = { currentUser: { id: string, permissions: { nodes: Array<{ namespace: string, action: string }> } } | null };

export type BlobQueryVariables = Exact<{
  repo: string;
  rev: string;
  path: string;
}>;


export type BlobQuery = { repository: { commit: { oid: string, blob: { path: string, name: string, url: string, canonicalURL: string, rawURL: string, byteSize: number, binary: boolean, contentTruncated: boolean, totalLines: number, languages: Array<string>, content: string, highlight: { aborted: boolean, packed: Array<number>, language: string | null, plainReason: string | null } } | null, tree: { url: string } | null } | null } | null };

export type CommitQueryVariables = Exact<{
  repo: string;
  rev: string;
}>;


export type CommitQuery = { repository: { commit: { oid: string, abbreviatedOID: string, subject: string, body: string | null, url: string, author: { date: string, person: { name: string, email: string } }, committer: { date: string, person: { name: string, email: string } } | null, parents: Array<{ oid: string, abbreviatedOID: string, url: string }>, tree: { url: string } | null, diff: { fileDiffs: { totalCount: number, diffStat: { added: number, deleted: number }, nodes: Array<{ oldPath: string | null, newPath: string | null, status: string, binary: boolean, tooLarge: boolean, stat: { added: number, deleted: number }, hunks: Array<{ body: string, oldRange: { startLine: number, lines: number }, newRange: { startLine: number, lines: number } }> }> } } } | null } | null };

export type CompareQueryVariables = Exact<{
  repo: string;
  base?: string | null | undefined;
  head?: string | null | undefined;
}>;


export type CompareQuery = { repository: { comparison: { range: { expr: string, mergeBase: { oid: string, abbreviatedOID: string } | null }, commits: { truncated: boolean, nodes: Array<{ oid: string, abbreviatedOID: string, subject: string, url: string, author: { date: string, person: { name: string, email: string } } }> }, fileDiffs: { totalCount: number, diffStat: { added: number, deleted: number }, nodes: Array<{ oldPath: string | null, newPath: string | null, status: string, binary: boolean, tooLarge: boolean, stat: { added: number, deleted: number }, hunks: Array<{ body: string, oldRange: { startLine: number, lines: number }, newRange: { startLine: number, lines: number } }> }> } } } | null };

export type GitRefsQueryVariables = Exact<{
  repo: string;
  type?: GitRefType | null | undefined;
  first: number;
  query?: string | null | undefined;
  order: GitRefOrder;
}>;


export type GitRefsQuery = { repository: { defaultBranch: { name: string } | null, gitRefs: { totalCount: number, nodes: Array<{ name: string, displayName: string, type: GitRefType, url: string, target: { oid: string, abbreviatedOID: string, commit: { subject: string, author: { date: string, person: { name: string } } } | null } }> } } | null };

export type HistoryQueryVariables = Exact<{
  repo: string;
  rev: string;
  path?: string | null | undefined;
  first: number;
  after?: string | null | undefined;
}>;


export type HistoryQuery = { repository: { commit: { ancestors: { nodes: Array<{ oid: string, abbreviatedOID: string, subject: string, url: string, author: { date: string, person: { name: string, email: string } } }>, pageInfo: { hasNextPage: boolean, endCursor: string | null } } } | null } | null };

export type RepositoriesQueryVariables = Exact<{
  first: number;
  after?: string | null | undefined;
  query?: string | null | undefined;
}>;


export type RepositoriesQuery = { repositories: { totalCount: number, pageInfo: { hasNextPage: boolean, endCursor: string | null }, nodes: Array<{ id: string, name: string, url: string, description: string, isPrivate: boolean, isFork: boolean, isArchived: boolean, mirrorInfo: { cloned: boolean, cloneInProgress: boolean, lastError: string | null } }> } };

export type RepositoryHeaderQueryVariables = Exact<{
  name: string;
}>;


export type RepositoryHeaderQuery = { repository: { id: string, name: string, url: string, description: string, externalURLs: Array<{ url: string, serviceKind: ExternalServiceKind | null }>, mirrorInfo: { cloned: boolean, cloneInProgress: boolean, isEmpty: boolean, remoteUnavailable: RemoteUnavailableReason | null }, defaultBranch: { displayName: string } | null } | null };

export type TreeEntriesQueryVariables = Exact<{
  repo: string;
  rev: string;
  path: string;
}>;


export type TreeEntriesQuery = { repository: { commit: { oid: string, tree: { url: string, archiveURL: string, entries: Array<
          | { __typename: 'GitBlob', name: string, path: string, isDirectory: boolean, kind: TreeEntryKind, url: string }
          | { __typename: 'GitTree', name: string, path: string, isDirectory: boolean, kind: TreeEntryKind, url: string }
        > } | null } | null } | null };

export type AdminRepositoriesQueryVariables = Exact<{
  first?: number | null | undefined;
  after?: string | null | undefined;
  query?: string | null | undefined;
  cloneStatus?: CloneStatus | null | undefined;
  failedFetch?: boolean | null | undefined;
  corrupted?: boolean | null | undefined;
  indexed?: boolean | null | undefined;
  notIndexed?: boolean | null | undefined;
}>;


export type AdminRepositoriesQuery = { repositoryStats: { total: number, cloned: number, cloning: number, notCloned: number, queued: number, failedFetch: number, noCodeAccess: number, indexed: number, notIndexed: number, corrupted: number, gitDirBytes: string }, statusMessages: Array<
    | { __typename: 'DeclaredConfigError' }
    | { __typename: 'GitUpdatesDisabled' }
    | { __typename: 'NoRepositoriesDetected' }
    | { __typename: 'CloningProgress' }
    | { __typename: 'ExternalServiceSyncError' }
    | { __typename: 'SyncError' }
    | { __typename: 'IndexingProgress' }
    | { __typename: 'GitserverDiskThresholdReached', message: string, address: string, clonesPaused: boolean }
  >, repositories: { totalCount: number, pageInfo: { hasNextPage: boolean, endCursor: string | null }, nodes: Array<{ id: string, name: string, url: string, mirrorInfo: { cloned: boolean, cloneInProgress: boolean, cloneProgress: string | null, isCorrupted: boolean, updatedAt: string | null, lastChanged: string | null, lastError: string | null, byteSize: string, shard: string | null, isEmpty: boolean, remoteUnavailable: RemoteUnavailableReason | null }, textSearchIndex: { lastIndexedAt: string | null, lastError: string | null } | null }> } };

export type AdminRepositoryQueryVariables = Exact<{
  name: string;
}>;


export type AdminRepositoryQuery = { repository: { id: string, name: string, url: string, mirrorInfo: { cloned: boolean, cloneInProgress: boolean, cloneProgress: string | null, isCorrupted: boolean, updatedAt: string | null, lastChanged: string | null, lastError: string | null, byteSize: string, shard: string | null, isEmpty: boolean, remoteUnavailable: RemoteUnavailableReason | null, remoteURL: string, nextSyncAt: string | null, corruptionLogs: Array<{ timestamp: string, reason: string }> }, externalServices: { nodes: Array<{ id: string, displayName: string }> }, textSearchIndex: { lastIndexedAt: string | null, lastError: string | null, lastErrorAt: string | null, queued: boolean, nodeError: string | null, status: { updatedAt: string, contentByteSize: string, contentFilesCount: number, indexByteSize: number, indexShardsCount: number, newLinesCount: number, defaultBranchNewLinesCount: number, otherBranchesNewLinesCount: number } | null, refs: Array<{ indexName: string, indexed: boolean, current: boolean, ref: { name: string, displayName: string, target: { oid: string } }, indexedCommit: { oid: string } | null }>, host: { name: string } | null, shards: Array<{ kind: string, file: string, byteSize: string, filesCount: number, repositoriesCount: number }> } | null } | null };

export type GitserversQueryVariables = Exact<{ [key: string]: never; }>;


export type GitserversQuery = { gitservers: { totalCount: number, nodes: Array<{ id: string, address: string, freeDiskSpaceBytes: string, totalDiskSpaceBytes: string, configured: boolean, error: string | null, repositoriesCount: number, assignedRepositoriesCount: number, repositoriesDiskBytes: string, repositoriesOnDisk: number | null, indexedRepositoriesCount: number, indexDiskBytes: string | null, clonePauseThresholdBytes: string | null, clonesPaused: boolean }> } };

export type RecloneRepositoryMutationVariables = Exact<{
  repo: string | number;
}>;


export type RecloneRepositoryMutation = { recloneRepository: { alwaysNil: string | null } };

export type ReindexRepositoryMutationVariables = Exact<{
  repository: string | number;
}>;


export type ReindexRepositoryMutation = { reindexRepository: { alwaysNil: string | null } };

export type RepoRebalanceQueryVariables = Exact<{ [key: string]: never; }>;


export type RepoRebalanceQuery = { repoRebalance: { waitingFor: Array<string>, queued: number, copying: number, flipped: number, failed: number, done: number, cancelled: number, nodes: Array<{ address: string, nodeList: Array<string>, alive: boolean, configured: boolean, lastSeenAt: string }>, moves: Array<{ repositoryName: string, repositoryId: string, fromNode: string, toNode: string, kind: string, state: string, attempts: number, bytesCopied: string, lastError: string | null, nextAttemptAt: string | null, startedAt: string | null, flippedAt: string | null, updatedAt: string }> } };

export type RetryFailedRepoMovesMutationVariables = Exact<{ [key: string]: never; }>;


export type RetryFailedRepoMovesMutation = { retryFailedRepoMoves: number };

export type UpdateMirrorRepositoryMutationVariables = Exact<{
  repository: string | number;
}>;


export type UpdateMirrorRepositoryMutation = { updateMirrorRepository: { alwaysNil: string | null } };

export type ChangeSavedSearchVisibilityMutationVariables = Exact<{
  id: string | number;
  newVisibility: SavedSearchVisibility;
}>;


export type ChangeSavedSearchVisibilityMutation = { changeSavedSearchVisibility: { id: string } };

export type CreateSavedSearchMutationVariables = Exact<{
  input: SavedSearchInput;
}>;


export type CreateSavedSearchMutation = { createSavedSearch: { id: string, url: string } };

export type CreateSearchContextMutationVariables = Exact<{
  searchContext: SearchContextInput;
  repositories: Array<SearchContextRepositoryRevisionsInput> | SearchContextRepositoryRevisionsInput;
}>;


export type CreateSearchContextMutation = { createSearchContext: { id: string, spec: string } };

export type DefaultSearchContextQueryVariables = Exact<{ [key: string]: never; }>;


export type DefaultSearchContextQuery = { defaultSearchContext: { id: string, spec: string } | null };

export type DeleteSavedSearchMutationVariables = Exact<{
  id: string | number;
}>;


export type DeleteSavedSearchMutation = { deleteSavedSearch: { alwaysNil: string | null } };

export type DeleteSearchContextMutationVariables = Exact<{
  id: string | number;
}>;


export type DeleteSearchContextMutation = { deleteSearchContext: { alwaysNil: string | null } };

export type RepositoryIdQueryVariables = Exact<{
  name: string;
}>;


export type RepositoryIdQuery = { repository: { id: string, name: string } | null };

export type RepositoryNamesQueryVariables = Exact<{
  query?: string | null | undefined;
  first?: number | null | undefined;
}>;


export type RepositoryNamesQuery = { repositories: { nodes: Array<{ id: string, name: string }> } };

export type SavedSearchQueryVariables = Exact<{
  id: string | number;
}>;


export type SavedSearchQuery = { node:
    | { __typename: 'BackgroundJob' }
    | { __typename: 'BatchChange' }
    | { __typename: 'BatchChangesCredential' }
    | { __typename: 'BatchSpec' }
    | { __typename: 'BulkOperation' }
    | { __typename: 'ChangesetEvent' }
    | { __typename: 'CodeIntelligenceConfigurationPolicy' }
    | { __typename: 'CodeownersIngestedFile' }
    | { __typename: 'ExecutorSecret' }
    | { __typename: 'ExecutorSecretAccessLog' }
    | { __typename: 'ExternalChangeset' }
    | { __typename: 'ExternalService' }
    | { __typename: 'HiddenBatchSpecWorkspace' }
    | { __typename: 'HiddenChangesetSpec' }
    | { __typename: 'HiddenExternalChangeset' }
    | { __typename: 'InsightView' }
    | { __typename: 'InsightsDashboard' }
    | { __typename: 'Monitor' }
    | { __typename: 'Notebook' }
    | { __typename: 'Org' }
    | { __typename: 'OrganizationInvitation' }
    | { __typename: 'OutOfBandMigration' }
    | { __typename: 'OutboundRequest' }
    | { __typename: 'OutboundWebhook' }
    | { __typename: 'OutboundWebhookLog' }
    | { __typename: 'PreciseIndex' }
    | { __typename: 'Repository' }
    | { __typename: 'SavedSearch', id: string, description: string, query: string, draft: boolean, visibility: SavedSearchVisibility, updatedAt: string, url: string, viewerCanAdminister: boolean, owner:
        | { __typename: 'Org', name: string, id: string }
        | { __typename: 'User', username: string, id: string }
      , createdBy: { username: string } | null }
    | { __typename: 'SearchContext' }
    | { __typename: 'SearchJob' }
    | { __typename: 'Team' }
    | { __typename: 'User' }
    | { __typename: 'VisibleBatchSpecWorkspace' }
    | { __typename: 'VisibleChangesetSpec' }
    | { __typename: 'WebhookLog' }
   | null };

export type SavedSearchesQueryVariables = Exact<{
  query?: string | null | undefined;
  owner?: string | number | null | undefined;
  first?: number | null | undefined;
  after?: string | null | undefined;
  orderBy: SavedSearchesOrderBy;
}>;


export type SavedSearchesQuery = { savedSearches: { totalCount: number, nodes: Array<{ id: string, description: string, query: string, draft: boolean, visibility: SavedSearchVisibility, updatedAt: string, url: string, viewerCanAdminister: boolean, owner:
        | { __typename: 'Org', name: string, id: string }
        | { __typename: 'User', username: string, id: string }
      , createdBy: { username: string } | null }>, pageInfo: { endCursor: string | null, hasNextPage: boolean } } };

export type SearchAggregateQueryVariables = Exact<{
  query: string;
  patternType: SearchPatternType;
  mode?: SearchAggregationMode | null | undefined;
  limit: number;
  extendedTimeout: boolean;
}>;


export type SearchAggregateQuery = { searchQueryAggregate: { modeAvailability: Array<{ mode: SearchAggregationMode, available: boolean, reasonUnavailable: string | null }>, aggregations:
      | { __typename: 'ExhaustiveSearchAggregationResult', mode: SearchAggregationMode, otherResultCount: number | null, otherGroupCount: number | null, groups: Array<{ label: string, count: number, query: string | null }> }
      | { __typename: 'NonExhaustiveSearchAggregationResult', mode: SearchAggregationMode, otherResultCount: number | null, approximateOtherGroupCount: number | null, groups: Array<{ label: string, count: number, query: string | null }> }
      | { __typename: 'SearchAggregationNotAvailable', mode: SearchAggregationMode, reason: string, reasonType: NotAvailableReasonType }
     } };

export type SearchContextBySpecQueryVariables = Exact<{
  spec: string;
}>;


export type SearchContextBySpecQuery = { searchContextBySpec: { id: string, name: string, spec: string, description: string, autoDefined: boolean, query: string, public: boolean, updatedAt: string, viewerCanManage: boolean, viewerHasAsDefault: boolean, viewerHasStarred: boolean, managedBy: string | null, namespace:
      | { __typename: 'Org', name: string, id: string }
      | { __typename: 'User', username: string, id: string }
     | null, repositories: Array<{ revisions: Array<string>, repository: { id: string, name: string } }> } | null };

export type SearchContextsQueryVariables = Exact<{
  first: number;
  after?: string | null | undefined;
  query?: string | null | undefined;
}>;


export type SearchContextsQuery = { searchContexts: { totalCount: number, nodes: Array<{ id: string, name: string, spec: string, description: string, autoDefined: boolean, query: string, public: boolean, updatedAt: string, viewerCanManage: boolean, viewerHasAsDefault: boolean, viewerHasStarred: boolean, managedBy: string | null, namespace:
        | { __typename: 'Org', name: string, id: string }
        | { __typename: 'User', username: string, id: string }
       | null }>, pageInfo: { endCursor: string | null, hasNextPage: boolean } } };

export type SetDefaultSearchContextMutationVariables = Exact<{
  searchContextID: string | number;
  userID: string | number;
}>;


export type SetDefaultSearchContextMutation = { setDefaultSearchContext: { alwaysNil: string | null } };

export type StarSearchContextMutationVariables = Exact<{
  searchContextID: string | number;
  userID: string | number;
}>;


export type StarSearchContextMutation = { createSearchContextStar: { alwaysNil: string | null } };

export type TransferSavedSearchMutationVariables = Exact<{
  id: string | number;
  newOwner: string | number;
}>;


export type TransferSavedSearchMutation = { transferSavedSearchOwnership: { id: string } };

export type UnstarSearchContextMutationVariables = Exact<{
  searchContextID: string | number;
  userID: string | number;
}>;


export type UnstarSearchContextMutation = { deleteSearchContextStar: { alwaysNil: string | null } };

export type UpdateSavedSearchMutationVariables = Exact<{
  id: string | number;
  input: SavedSearchUpdateInput;
}>;


export type UpdateSavedSearchMutation = { updateSavedSearch: { id: string } };

export type UpdateSearchContextMutationVariables = Exact<{
  id: string | number;
  searchContext: SearchContextEditInput;
  repositories: Array<SearchContextRepositoryRevisionsInput> | SearchContextRepositoryRevisionsInput;
}>;


export type UpdateSearchContextMutation = { updateSearchContext: { id: string, spec: string } };

export type ViewerNamespacesQueryVariables = Exact<{ [key: string]: never; }>;


export type ViewerNamespacesQuery = { currentUser: { id: string, username: string, siteAdmin: boolean, organizations: { nodes: Array<{ id: string, name: string }> } } | null };

export type EditTemporarySettingsMutationVariables = Exact<{
  contents: string;
}>;


export type EditTemporarySettingsMutation = { editTemporarySettings: { alwaysNil: string | null } };

export type TemporarySettingsQueryVariables = Exact<{ [key: string]: never; }>;


export type TemporarySettingsQuery = { temporarySettings: { contents: string } };

export type CancelSearchJobMutationVariables = Exact<{
  id: string | number;
}>;


export type CancelSearchJobMutation = { cancelSearchJob: { alwaysNil: string | null } | null };

export type CreateSearchJobMutationVariables = Exact<{
  query: string;
}>;


export type CreateSearchJobMutation = { createSearchJob: { id: string, query: string, state: SearchJobState, createdAt: string, startedAt: string | null, finishedAt: string | null, URL: string | null, logURL: string | null, creator: { username: string } | null, repoStats: { total: number, completed: number, failed: number, inProgress: number } } };

export type DeleteSearchJobMutationVariables = Exact<{
  id: string | number;
}>;


export type DeleteSearchJobMutation = { deleteSearchJob: { alwaysNil: string | null } };

export type SearchJobsQueryVariables = Exact<{
  first?: number | null | undefined;
  after?: string | null | undefined;
  query?: string | null | undefined;
  states?: Array<SearchJobState> | SearchJobState | null | undefined;
  orderBy?: SearchJobsOrderBy | null | undefined;
  descending?: boolean | null | undefined;
}>;


export type SearchJobsQuery = { searchJobs: { totalCount: number, nodes: Array<{ id: string, query: string, state: SearchJobState, createdAt: string, startedAt: string | null, finishedAt: string | null, URL: string | null, logURL: string | null, creator: { username: string } | null, repoStats: { total: number, completed: number, failed: number, inProgress: number } }>, pageInfo: { endCursor: string | null, hasNextPage: boolean } } };

export type ValidateSearchJobQueryVariables = Exact<{
  query: string;
}>;


export type ValidateSearchJobQuery = { validateSearchJob: { alwaysNil: string | null } | null };

export type StatusMessagesQueryVariables = Exact<{ [key: string]: never; }>;


export type StatusMessagesQuery = { statusMessages: Array<
    | { __typename: 'DeclaredConfigError', message: string, source: string, path: string }
    | { __typename: 'GitUpdatesDisabled', message: string }
    | { __typename: 'NoRepositoriesDetected', message: string }
    | { __typename: 'CloningProgress', message: string, cloned: number, cloning: number, queued: number, total: number }
    | { __typename: 'ExternalServiceSyncError', message: string, externalService: { id: string, displayName: string } }
    | { __typename: 'SyncError', message: string }
    | { __typename: 'IndexingProgress', notIndexed: number, indexed: number }
    | { __typename: 'GitserverDiskThresholdReached', message: string, address: string, clonesPaused: boolean }
  > };

export type OutOfBandMigrationsQueryVariables = Exact<{ [key: string]: never; }>;


export type OutOfBandMigrationsQuery = { outOfBandMigrations: Array<{ id: string, team: string, component: string, description: string, introduced: string, deprecated: string | null, progress: number, created: string, lastUpdated: string | null, nonDestructive: boolean, applyReverse: boolean, errors: Array<{ message: string, created: string }> }> };

export type SetMigrationDirectionMutationVariables = Exact<{
  id: string | number;
  applyReverse: boolean;
}>;


export type SetMigrationDirectionMutation = { setMigrationDirection: { alwaysNil: string | null } };

export type UpgradeReadinessQueryVariables = Exact<{ [key: string]: never; }>;


export type UpgradeReadinessQuery = { site: { productVersion: string, upgradeReadiness: { schemaVersion: string | null, schemaDriftNote: string | null, schemaDrift: Array<{ name: string, problem: string, solution: string, diff: string | null, statements: Array<string> | null, urlHint: string | null }>, requiredOutOfBandMigrations: Array<{ id: string, description: string, progress: number, deprecated: string | null, applyReverse: boolean }> } } };
