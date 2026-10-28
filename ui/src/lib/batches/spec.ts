/**
 * Bulk change spec authoring for server-side runs: creating a bulk change, the spec editor
 * (validation with positions, the example library) and the workspace preview. GraphQL operations
 * and the pure helpers the pages use; the execution views are in `execution.ts`.
 */
import { graphql, GraphQLRequestError } from '#lib/graphql.ts';
import type { NamespaceKind } from './batch-change.ts';
import eslintFix from './examples/eslint-fix.batch.yaml?raw';
import goDependencyBump from './examples/go-dependency-bump.batch.yaml?raw';
import helloWorld from './examples/hello-world.batch.yaml?raw';
import modifyYaml from './examples/modify-yaml-files.batch.yaml?raw';
import outputsAndTemplates from './examples/outputs-and-templates.batch.yaml?raw';
import sedReplace from './examples/sed-replace.batch.yaml?raw';
import workspaceDiscovery from './examples/workspace-discovery.batch.yaml?raw';
import type {
	BatchChangeForEditQuery,
	BatchesViewerQuery,
	BatchOrgNamespaceQuery,
	BatchUserNamespaceQuery,
	CreateEmptyBatchChangeMutation,
	EditorSpecQuery,
	WorkspacesPreviewQuery
} from '#lib/graphql-types.ts';

// ------------------------------------------------------------------ access

/** Error codes the bulk changes API answers with when the feature is off for the viewer. */
export const DISABLED_CODES = ['ErrBatchChangesDisabled', 'ErrBatchChangesDisabledForUser'];

/** The signed-in user with what decides the bulk changes actions they get. */
export const BATCHES_VIEWER = `query BatchesViewer {
	currentUser {
		id
		username
		siteAdmin
		url
		permissions { nodes { namespace action } }
		organizations { nodes { id name displayName url } }
	}
}`;

/** Fails with a `DISABLED_CODES` or `FORBIDDEN` error when bulk changes are off for the viewer. */
export const BATCHES_PROBE = `query BatchesProbe { batchChanges(first: 1) { totalCount } }`;

export interface BatchesViewer {
	id: string;
	username: string;
	siteAdmin: boolean;
	url: string;
	permissions: { nodes: { namespace: string; action: string }[] };
	organizations: { nodes: { id: string; name: string; displayName: string | null; url: string }[] };
}

export type BatchesViewerResult = BatchesViewerQuery;

/** Whether the viewer holds `BATCH_CHANGES#WRITE` (needed for every mutation here). */
export function canWriteBatchChanges(viewer: Pick<BatchesViewer, 'permissions'> | null): boolean {
	return (
		viewer?.permissions.nodes.some((p) => p.namespace === 'BATCH_CHANGES' && p.action === 'WRITE') ?? false
	);
}

/**
 * The message to show instead of a bulk changes page when the API refuses the viewer
 * (feature disabled, restricted to admins, or no `BATCH_CHANGES#READ`); null for other errors.
 */
export function batchesUnavailableMessage(err: unknown): string | null {
	if (!(err instanceof GraphQLRequestError)) return null;
	if (DISABLED_CODES.some((code) => err.hasCode(code))) return err.errors[0]?.message ?? err.message;
	if (err.hasCode('FORBIDDEN')) return 'You do not have permission to use bulk changes.';
	return null;
}

/** What the bulk changes pages may offer the viewer. */
export type BatchesAccess =
	| { available: true; viewer: BatchesViewer; canWrite: boolean }
	| { available: false; message: string };

/**
 * Loads the viewer and checks that bulk changes are available to them (the API answers with
 * an error code otherwise). Other errors are thrown.
 */
export async function loadBatchesAccess(): Promise<BatchesAccess> {
	const { currentUser } = await graphql<BatchesViewerResult>(BATCHES_VIEWER);
	if (!currentUser) return { available: false, message: 'Sign in to use bulk changes.' };
	try {
		await graphql(BATCHES_PROBE);
	} catch (err) {
		const message = batchesUnavailableMessage(err);
		if (message === null) throw err;
		return { available: false, message };
	}
	return { available: true, viewer: currentUser, canWrite: canWriteBatchChanges(currentUser) };
}

// ------------------------------------------------------------------ namespaces and URLs

export type { NamespaceKind };


export const USER_NAMESPACE = `query BatchUserNamespace($name: String!) { user(username: $name) { id url } }`;
export const ORG_NAMESPACE = `query BatchOrgNamespace($name: String!) { organization(name: $name) { id url } }`;

export type NamespaceResult = Partial<BatchUserNamespaceQuery & BatchOrgNamespaceQuery>;

/** The spec editor of a bulk change (its `url` is `/users/<name>/bulk-changes/<name>`). */
export function editURL(batchChangeURL: string): string {
	return `${batchChangeURL}/edit`;
}

/** The execution page of a server-side spec of a bulk change. */
export function executionURL(batchChangeURL: string, specID: string): string {
	return `${batchChangeURL}/executions/${encodeURIComponent(specID)}`;
}

/** A bulk change's URL from the route parameters. */
export function batchChangeURLFromRoute(kind: NamespaceKind, namespace: string, name: string): string {
	return `/${kind}/${encodeURIComponent(namespace)}/bulk-changes/${encodeURIComponent(name)}`;
}

// ------------------------------------------------------------------ create

/** Names are valid in a spec: word characters, dots and dashes. */
export function isValidBatchChangeName(name: string): boolean {
	return /^[\w.-]+$/.test(name);
}

export const CREATE_EMPTY_BATCH_CHANGE = `mutation CreateEmptyBatchChange($namespace: ID!, $name: String!) {
	createEmptyBatchChange(namespace: $namespace, name: $name) { id name url }
}`;

export type CreateEmptyBatchChangeResult = CreateEmptyBatchChangeMutation;

// ------------------------------------------------------------------ specs

const SPEC_FIELDS = `fragment SpecEditorFields on BatchSpec {
	id
	state
	source
	originalInput
	createdAt
	startedAt
	finishedAt
	applyURL
	viewerCanAdminister
	allowIgnored
	allowUnsupported
	noCache
	failureMessage
	creator { id username }
	workspaceResolution {
		state
		failureMessage
		startedAt
		finishedAt
		workspaces(first: 0) { totalCount stats { errored completed processing queued ignored } }
		skipped: workspaces(first: 0, state: SKIPPED) { totalCount }
	}
}`;

export type BatchSpecState =
	| 'PENDING'
	| 'PROCESSING'
	| 'FAILED'
	| 'COMPLETED'
	| 'QUEUED'
	| 'CANCELING'
	| 'CANCELED';

export type ResolutionState = 'QUEUED' | 'PROCESSING' | 'ERRORED' | 'FAILED' | 'COMPLETED';

export interface WorkspaceStats {
	errored: number;
	completed: number;
	processing: number;
	queued: number;
	ignored: number;
}

export interface EditorSpec {
	id: string;
	state: BatchSpecState;
	source: 'LOCAL' | 'REMOTE';
	originalInput: string;
	createdAt: string;
	startedAt: string | null;
	finishedAt: string | null;
	applyURL: string | null;
	viewerCanAdminister: boolean;
	allowIgnored: boolean | null;
	allowUnsupported: boolean | null;
	noCache: boolean | null;
	failureMessage: string | null;
	creator: { id: string; username: string } | null;
	workspaceResolution: {
		state: ResolutionState;
		failureMessage: string | null;
		startedAt: string | null;
		finishedAt: string | null;
		workspaces: { totalCount: number; stats: WorkspaceStats };
		/** Workspaces that do not run (ignored, unsupported, or no step runs). */
		skipped: { totalCount: number };
	} | null;
}

/** The bulk change being edited, with its current spec and newest server-side spec. */
export const BATCH_CHANGE_FOR_EDIT = `query BatchChangeForEdit($namespace: ID!, $name: String!) {
	batchChange(namespace: $namespace, name: $name) {
		id
		name
		state
		url
		viewerCanAdminister
		currentSpec { ...SpecEditorFields }
		batchSpecs(first: 1) { nodes { ...SpecEditorFields } }
	}
}
${SPEC_FIELDS}`;

export interface EditorBatchChange {
	id: string;
	name: string;
	state: 'OPEN' | 'CLOSED' | 'DRAFT';
	url: string;
	viewerCanAdminister: boolean;
	currentSpec: EditorSpec;
	batchSpecs: { nodes: EditorSpec[] };
}

export type BatchChangeForEditResult = BatchChangeForEditQuery;

/** One spec, for polling its resolution. */
export const EDITOR_SPEC = `query EditorSpec($id: ID!) {
	node(id: $id) { __typename ...SpecEditorFields }
}
${SPEC_FIELDS}`;

export type EditorSpecResult = EditorSpecQuery;

export const CREATE_BATCH_SPEC_FROM_RAW = `mutation CreateBatchSpecFromRaw(
	$spec: String!
	$namespace: ID!
	$batchChange: ID!
	$allowIgnored: Boolean
	$allowUnsupported: Boolean
	$noCache: Boolean
) {
	createBatchSpecFromRaw(
		batchSpec: $spec
		namespace: $namespace
		batchChange: $batchChange
		allowIgnored: $allowIgnored
		allowUnsupported: $allowUnsupported
		noCache: $noCache
	) { ...SpecEditorFields }
}
${SPEC_FIELDS}`;

export const REPLACE_BATCH_SPEC_INPUT = `mutation ReplaceBatchSpecInput(
	$previousSpec: ID!
	$spec: String!
	$allowIgnored: Boolean
	$allowUnsupported: Boolean
	$noCache: Boolean
) {
	replaceBatchSpecInput(
		previousSpec: $previousSpec
		batchSpec: $spec
		allowIgnored: $allowIgnored
		allowUnsupported: $allowUnsupported
		noCache: $noCache
	) { ...SpecEditorFields }
}
${SPEC_FIELDS}`;

export interface SpecFlags {
	allowIgnored: boolean;
	allowUnsupported: boolean;
	noCache: boolean;
}

export const EXECUTE_BATCH_SPEC = `mutation ExecuteBatchSpec($batchSpec: ID!, $noCache: Boolean) {
	executeBatchSpec(batchSpec: $batchSpec, noCache: $noCache) { id state }
}`;

/**
 * Whether a server-side spec has been executed (or is executing): it then stays as it ran, and
 * edits go to a new spec.
 */
export function isSpecExecuted(spec: Pick<EditorSpec, 'startedAt' | 'state' | 'workspaceResolution'>): boolean {
	if (spec.startedAt) return true;
	if (['QUEUED', 'PROCESSING', 'CANCELING', 'CANCELED'].includes(spec.state)) return true;
	const s = spec.workspaceResolution?.workspaces.stats;
	return !!s && s.queued + s.processing + s.completed + s.errored > 0;
}

/**
 * The spec the editor works on: the newest server-side spec of the bulk change, else its
 * current spec (a new draft's empty spec).
 */
export function latestSpec(bc: Pick<EditorBatchChange, 'currentSpec' | 'batchSpecs'>): EditorSpec {
	const newest = bc.batchSpecs.nodes[0];
	if (newest && newest.createdAt >= bc.currentSpec.createdAt) return newest;
	return bc.currentSpec;
}

/**
 * How an edit is saved: the spec's input is replaced while it is a
 * server-side spec that has not run and is not the applied spec of a published bulk change;
 * otherwise a new spec is created for the bulk change.
 */
export function specSaveMode(
	spec: Pick<EditorSpec, 'id' | 'source' | 'startedAt' | 'state' | 'workspaceResolution'>,
	bc: Pick<EditorBatchChange, 'state' | 'currentSpec'>
): 'replace' | 'create' {
	if (spec.source !== 'REMOTE') return 'create';
	if (bc.state !== 'DRAFT' && spec.id === bc.currentSpec.id) return 'create';
	return isSpecExecuted(spec) ? 'create' : 'replace';
}

// ------------------------------------------------------------------ validation errors

/** One problem with a spec, with its position when known (`extensions.issues`). */
export interface SpecIssue {
	message: string;
	path: string | null;
	line: number | null;
	column: number | null;
}

/** `line L, column C: message` (the server's message format). */
const POSITIONED = /^\s*(?:-\s*)?line (\d+), column (\d+): (.*)$/;

/**
 * The issues of a failed `createBatchSpecFromRaw`/`replaceBatchSpecInput`: from
 * `extensions.issues` when present, else parsed from the messages. Null when the error is not
 * a validation error (a network failure, permissions, …).
 */
export function specIssues(err: unknown): SpecIssue[] | null {
	if (!(err instanceof GraphQLRequestError)) return null;
	const issues: SpecIssue[] = [];
	for (const e of err.errors) {
		const ext = e.extensions?.['issues'];
		if (Array.isArray(ext)) {
			for (const raw of ext as Record<string, unknown>[]) {
				issues.push({
					message: String(raw['message'] ?? ''),
					path: typeof raw['path'] === 'string' && raw['path'] !== '' ? raw['path'] : null,
					line: typeof raw['line'] === 'number' ? raw['line'] : null,
					column: typeof raw['column'] === 'number' ? raw['column'] : null
				});
			}
		} else if (e.extensions?.['code'] === 'VALIDATION') {
			for (const text of e.message.split('\n')) {
				if (/^The input is invalid:?$/.test(text.trim()) || text.trim() === '') continue;
				const m = POSITIONED.exec(text);
				if (m) issues.push({ message: m[3] ?? '', path: null, line: Number(m[1]), column: Number(m[2]) });
				else issues.push({ message: text.replace(/^\s*-\s*/, ''), path: null, line: null, column: null });
			}
		}
	}
	return issues.length > 0 ? issues : null;
}

/** `line 3, column 5: …` for the list under the editor (the message names the field). */
export function formatIssue(issue: SpecIssue): string {
	const at = issue.line !== null ? `line ${issue.line}${issue.column !== null ? `, column ${issue.column}` : ''}: ` : '';
	return `${at}${issue.message}`;
}

/**
 * The document range an issue marks: from its line and column (1-based) to the end of that
 * line; the start of the document without a position. Positions past the end are clamped.
 */
export function issueRange(text: string, issue: Pick<SpecIssue, 'line' | 'column'>): { from: number; to: number } {
	if (issue.line === null) return { from: 0, to: Math.min(text.length, text.indexOf('\n') >>> 0) };
	const lines = text.split('\n');
	const index = Math.min(Math.max(issue.line, 1), lines.length) - 1;
	let start = 0;
	for (let i = 0; i < index; i++) start += (lines[i]?.length ?? 0) + 1;
	const line = lines[index] ?? '';
	const col = Math.min(Math.max((issue.column ?? 1) - 1, 0), line.length);
	const from = start + col;
	const to = start + line.length;
	return { from, to: to > from ? to : Math.min(from + 1, text.length) };
}

// ------------------------------------------------------------------ spec text

const NAME_LINE = /^name:[ \t]*(["']?)([^\s"'#]*)\1[ \t]*(#.*)?$/m;

/** The top-level `name:` of a spec, if it is a plain value. */
export function specName(text: string): string | null {
	const m = NAME_LINE.exec(text);
	return m?.[2] ? m[2] : null;
}

/** Sets the top-level `name:` of a spec (added at the top when missing). */
export function withSpecName(text: string, name: string): string {
	if (/^name:.*$/m.test(text)) return text.replace(/^name:.*$/m, `name: ${name}`);
	return `name: ${name}\n${text}`;
}

/** Whether a spec has nothing but a name (the spec of a new, empty bulk change). */
export function isEmptySpec(text: string): boolean {
	return text
		.split('\n')
		.map((l) => l.replace(/#.*$/, '').trim())
		.every((l) => l === '' || /^name:/.test(l) || /^version:/.test(l));
}

/** One example of the editor's library. */
export interface SpecExample {
	id: string;
	title: string;
	description: string;
	code: string;
}

/**
 * The library of example specs. Inserting one sets its name to the bulk change's.
 */
export const SPEC_EXAMPLES: SpecExample[] = [
	{ id: 'hello-world', title: 'Hello world', description: 'Append a line to every README.md.', code: helloWorld },
	{ id: 'sed-replace', title: 'Replace words with sed', description: 'Search and replace terms in Markdown files.', code: sedReplace },
	{ id: 'go-dependency-bump', title: 'Upgrade a Go dependency', description: 'Run go get and go mod tidy where a module is used.', code: goDependencyBump },
	{ id: 'eslint-fix', title: 'Run eslint --fix', description: 'Fix lint errors in JavaScript and TypeScript repositories.', code: eslintFix },
	{ id: 'workspace-discovery', title: 'Monorepo workspaces', description: 'One changeset per project directory (rootAtLocationOf).', code: workspaceDiscovery },
	{ id: 'outputs-and-templates', title: 'Outputs and templates', description: 'Step outputs in YAML used in the changeset template.', code: outputsAndTemplates },
	{ id: 'modify-yaml-files', title: 'Modify YAML files', description: 'Edit files found by the search with a script mounted into the container.', code: modifyYaml }
];

/** The spec a new bulk change starts with: the hello world example with its name. */
export function initialSpec(name: string): string {
	return withSpecName(helloWorld, name);
}

// ------------------------------------------------------------------ workspace preview

export const WORKSPACES_PREVIEW = `query WorkspacesPreview($id: ID!, $first: Int, $after: String, $search: String) {
	node(id: $id) {
		__typename
		... on BatchSpec {
			id
			workspaceResolution {
				state
				failureMessage
				workspaces(first: $first, after: $after, search: $search) {
					totalCount
					pageInfo { hasNextPage endCursor }
					stats { errored completed processing queued ignored }
					nodes {
						__typename
						id
						state
						ignored
						unsupported
						cachedResultFound
						stepCacheResultCount
						onlyFetchWorkspace
						... on VisibleBatchSpecWorkspace {
							repository { name url }
							branch { displayName }
							path
							searchResultPaths
						}
					}
				}
			}
		}
	}
}`;

export interface PreviewWorkspace {
	__typename: 'VisibleBatchSpecWorkspace' | 'HiddenBatchSpecWorkspace';
	id: string;
	state: string;
	ignored: boolean;
	unsupported: boolean;
	cachedResultFound: boolean;
	stepCacheResultCount: number;
	onlyFetchWorkspace: boolean;
	repository?: { name: string; url: string };
	branch?: { displayName: string };
	path?: string;
	searchResultPaths?: string[];
}

export type WorkspacesPreviewResult = WorkspacesPreviewQuery;

/** Why a workspace will not run (empty when it will). */
export function skipReasons(ws: Pick<PreviewWorkspace, 'ignored' | 'unsupported' | 'state'>, flags?: Partial<SpecFlags>): string[] {
	const reasons: string[] = [];
	if (ws.ignored && !flags?.allowIgnored) reasons.push('The repository has a .batchignore file.');
	if (ws.unsupported && !flags?.allowUnsupported) reasons.push("The repository's code host is not supported.");
	if (reasons.length === 0 && ws.state === 'SKIPPED') reasons.push('No step runs in this workspace.');
	return reasons;
}

/** Whether the resolution is still running (the preview polls meanwhile). */
export function isResolving(state: ResolutionState | null | undefined): boolean {
	return state === 'QUEUED' || state === 'PROCESSING';
}

/** A sentence for the resolution's state. */
export function resolutionLabel(state: ResolutionState | null | undefined): string {
	switch (state) {
		case 'QUEUED':
			return 'Waiting to resolve workspaces…';
		case 'PROCESSING':
			return 'Resolving workspaces…';
		case 'ERRORED':
			return 'Resolving workspaces failed; retrying…';
		case 'FAILED':
			return 'Resolving workspaces failed.';
		case 'COMPLETED':
			return 'Workspaces resolved.';
		default:
			return 'Workspaces are resolved once the spec is valid.';
	}
}

/**
 * Whether the spec can be run: resolved, with at least one workspace that is not skipped, by
 * someone who may administer the namespace.
 */
export function canRun(spec: Pick<EditorSpec, 'viewerCanAdminister' | 'workspaceResolution'>): boolean {
	const r = spec.workspaceResolution;
	if (!spec.viewerCanAdminister || r?.state !== 'COMPLETED') return false;
	return r.workspaces.totalCount - r.skipped.totalCount > 0;
}
