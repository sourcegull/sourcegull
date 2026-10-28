/**
 * Preview and apply of a bulk change spec: `BatchSpec.applyPreview` with its
 * filters and stats, per-changeset publication states chosen in the UI, and the apply mutations.
 */
import type { DiffStat, NamespaceRef } from './batch-change.ts';
import type { ChangesetState } from './changesets.ts';
import type {
	ApplyBatchChangeFromUiMutation,
	BatchSpecApplyPreviewQuery,
	BatchSpecForApplyQuery,
	CreateBatchChangeFromUiMutation
} from '#lib/graphql-types.ts';

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

export const OPERATIONS: ChangesetSpecOperation[] = [
	'IMPORT',
	'PUBLISH',
	'PUBLISH_DRAFT',
	'PUSH',
	'UPDATE',
	'UNDRAFT',
	'CLOSE',
	'REOPEN',
	'SLEEP',
	'SYNC',
	'DETACH',
	'ARCHIVE',
	'REATTACH'
];

/** The label of an operation badge in the preview. */
export function operationLabel(op: ChangesetSpecOperation): string {
	switch (op) {
		case 'PUBLISH':
			return 'Publish';
		case 'PUBLISH_DRAFT':
			return 'Publish draft';
		case 'PUSH':
			return 'Push';
		case 'UPDATE':
			return 'Update';
		case 'UNDRAFT':
			return 'Undraft';
		case 'IMPORT':
			return 'Import';
		case 'CLOSE':
			return 'Close';
		case 'REOPEN':
			return 'Reopen';
		case 'SLEEP':
			return 'Sleep';
		case 'SYNC':
			return 'Sync';
		case 'DETACH':
			return 'Detach';
		case 'ARCHIVE':
			return 'Archive';
		case 'REATTACH':
			return 'Reattach';
	}
}

export interface ChangesetSpecDelta {
	titleChanged: boolean;
	bodyChanged: boolean;
	undraft: boolean;
	baseRefChanged: boolean;
	diffChanged: boolean;
	commitMessageChanged: boolean;
	authorNameChanged: boolean;
	authorEmailChanged: boolean;
}

/** What changes in an update, as short phrases ("title", "diff", ...). */
export function deltaSummary(d: ChangesetSpecDelta): string[] {
	const out: string[] = [];
	if (d.titleChanged) out.push('title');
	if (d.bodyChanged) out.push('description');
	if (d.baseRefChanged) out.push('base branch');
	if (d.diffChanged) out.push('diff');
	if (d.commitMessageChanged) out.push('commit message');
	if (d.authorNameChanged || d.authorEmailChanged) out.push('commit author');
	if (d.undraft) out.push('undraft');
	return out;
}

// ---- Bulk change spec ------------------------------------------------------------------------------

export const BATCH_SPEC = `query BatchSpecForApply($id: ID!) {
	node(id: $id) {
		__typename
		... on BatchSpec {
			id state source createdAt expiresAt applyURL viewerCanAdminister
			description { name description }
			namespace { __typename id url ... on User { username } ... on Org { name } }
			creator { username }
			diffStat { added deleted }
			appliesToBatchChange { id name url state }
			changesetSpecs(first: 1) { totalCount }
		}
	}
}`;

export interface BatchSpecForApply {
	__typename: 'BatchSpec';
	id: string;
	state: string;
	source: 'LOCAL' | 'REMOTE';
	createdAt: string;
	expiresAt: string | null;
	applyURL: string | null;
	viewerCanAdminister: boolean;
	description: { name: string; description: string };
	namespace: NamespaceRef;
	creator: { username: string } | null;
	diffStat: DiffStat | null;
	appliesToBatchChange: { id: string; name: string; url: string; state: string } | null;
	changesetSpecs: { totalCount: number };
}

export type BatchSpecResult = BatchSpecForApplyQuery;

// ---- Preview ---------------------------------------------------------------------------------

/** `true`, `false` or `"draft"`. */
export type PublishedValue = boolean | 'draft';

export interface PublicationStateInput {
	changesetSpec: string;
	publicationState: PublishedValue;
}

const SPEC_FIELDS = `
	id type
	description {
		__typename
		... on GitBranchChangesetDescription {
			baseRepository { name url }
			baseRef headRef title body published
			diffStat { added deleted }
		}
		... on ExistingChangesetReference { baseRepository { name url } externalID }
	}`;

const CHANGESET_FIELDS = `id state title externalID externalURL { url } repository { name url }`;

export const APPLY_PREVIEW = `query BatchSpecApplyPreview($batchSpec: ID!, $first: Int, $after: String, $search: String, $currentState: ChangesetState, $action: ChangesetSpecOperation, $publicationStates: [ChangesetSpecPublicationStateInput!]) {
	node(id: $batchSpec) {
		... on BatchSpec {
			applyPreview(first: $first, after: $after, search: $search, currentState: $currentState, action: $action, publicationStates: $publicationStates) {
				totalCount
				pageInfo { hasNextPage endCursor }
				stats {
					push update undraft publish publishDraft sync import close reopen sleep detach archive reattach
					added modified removed
				}
				nodes {
					__typename
					... on VisibleChangesetApplyPreview {
						operations
						delta { titleChanged bodyChanged undraft baseRefChanged diffChanged commitMessageChanged authorNameChanged authorEmailChanged }
						targets {
							__typename
							... on VisibleApplyPreviewTargetsAttach { changesetSpec { ${SPEC_FIELDS} } }
							... on VisibleApplyPreviewTargetsUpdate {
								changesetSpec { ${SPEC_FIELDS} }
								changeset { ${CHANGESET_FIELDS} }
							}
							... on VisibleApplyPreviewTargetsDetach { changeset { ${CHANGESET_FIELDS} } }
						}
					}
					... on HiddenChangesetApplyPreview {
						operations
						targets { __typename }
					}
				}
			}
		}
	}
}`;

export interface PreviewSpec {
	id: string;
	type: 'BRANCH' | 'EXISTING';
	description:
		| {
				__typename: 'GitBranchChangesetDescription';
				baseRepository: { name: string; url: string };
				baseRef: string;
				headRef: string;
				title: string;
				body: string;
				published: PublishedValue | null;
				diffStat: DiffStat;
		  }
		| {
				__typename: 'ExistingChangesetReference';
				baseRepository: { name: string; url: string };
				externalID: string;
		  };
}

export interface PreviewChangeset {
	id: string;
	state: ChangesetState;
	title: string | null;
	externalID: string | null;
	externalURL: { url: string } | null;
	repository: { name: string; url: string };
}

export type PreviewTargets =
	| { __typename: 'VisibleApplyPreviewTargetsAttach'; changesetSpec: PreviewSpec }
	| { __typename: 'VisibleApplyPreviewTargetsUpdate'; changesetSpec: PreviewSpec; changeset: PreviewChangeset }
	| { __typename: 'VisibleApplyPreviewTargetsDetach'; changeset: PreviewChangeset };

export type PreviewNode =
	| {
			__typename: 'VisibleChangesetApplyPreview';
			operations: ChangesetSpecOperation[];
			delta: ChangesetSpecDelta;
			targets: PreviewTargets;
	  }
	| {
			__typename: 'HiddenChangesetApplyPreview';
			operations: ChangesetSpecOperation[];
			targets: { __typename: string };
	  };

export interface PreviewStats {
	push: number;
	update: number;
	undraft: number;
	publish: number;
	publishDraft: number;
	sync: number;
	import: number;
	close: number;
	reopen: number;
	sleep: number;
	detach: number;
	archive: number;
	reattach: number;
	added: number;
	modified: number;
	removed: number;
}

export type ApplyPreviewResult = BatchSpecApplyPreviewQuery;

/** The spec of a preview entry, if it has one (attach and update). */
export function previewSpec(t: PreviewTargets): PreviewSpec | null {
	return t.__typename === 'VisibleApplyPreviewTargetsDetach' ? null : t.changesetSpec;
}

/** The title, repository and branch shown for a preview entry. */
export function previewTitle(t: PreviewTargets): { title: string; repo: string; branch: string | null } {
	const spec = previewSpec(t);
	if (spec?.description.__typename === 'GitBranchChangesetDescription') {
		const d = spec.description;
		return { title: d.title, repo: d.baseRepository.name, branch: d.headRef };
	}
	if (spec?.description.__typename === 'ExistingChangesetReference') {
		const d = spec.description;
		const title = t.__typename === 'VisibleApplyPreviewTargetsUpdate' ? t.changeset.title : null;
		return { title: title ?? `Import !${d.externalID}`, repo: d.baseRepository.name, branch: null };
	}
	const c = (t as { changeset: PreviewChangeset }).changeset;
	return { title: c.title ?? `Changeset ${c.externalID ?? ''}`.trim(), repo: c.repository.name, branch: null };
}

/**
 * Whether the UI may choose the publication state of an entry's spec: branch specs whose
 * `published` is unset (the server rejects the others).
 */
export function canSetPublicationState(t: PreviewTargets): boolean {
	const spec = previewSpec(t);
	return (
		spec !== null &&
		spec.description.__typename === 'GitBranchChangesetDescription' &&
		spec.description.published === null
	);
}

/** The `publicationStates` argument from the choices made in the UI (spec ID → value). */
export function publicationStatesInput(choices: ReadonlyMap<string, PublishedValue>): PublicationStateInput[] {
	return [...choices].map(([changesetSpec, publicationState]) => ({ changesetSpec, publicationState }));
}

/** The label of a publication state choice. */
export function publishedLabel(v: PublishedValue | null): string {
	if (v === 'draft') return 'Draft';
	if (v === true) return 'Published';
	if (v === false) return 'Unpublished';
	return 'Not set';
}

/** The stats' operation counts worth showing, in `OPERATIONS` order. */
export function statsOperations(s: PreviewStats): { op: ChangesetSpecOperation; count: number }[] {
	const counts: Record<ChangesetSpecOperation, number> = {
		IMPORT: s.import,
		PUBLISH: s.publish,
		PUBLISH_DRAFT: s.publishDraft,
		PUSH: s.push,
		UPDATE: s.update,
		UNDRAFT: s.undraft,
		CLOSE: s.close,
		REOPEN: s.reopen,
		SLEEP: s.sleep,
		SYNC: s.sync,
		DETACH: s.detach,
		ARCHIVE: s.archive,
		REATTACH: s.reattach
	};
	return OPERATIONS.filter((op) => counts[op] > 0).map((op) => ({ op, count: counts[op] }));
}

// ---- Apply -----------------------------------------------------------------------------------

export const APPLY_BATCH_CHANGE = `mutation ApplyBatchChangeFromUI($batchSpec: ID!, $ensureBatchChange: ID, $publicationStates: [ChangesetSpecPublicationStateInput!]) {
	applyBatchChange(batchSpec: $batchSpec, ensureBatchChange: $ensureBatchChange, publicationStates: $publicationStates) { id url }
}`;

export const CREATE_BATCH_CHANGE = `mutation CreateBatchChangeFromUI($batchSpec: ID!, $publicationStates: [ChangesetSpecPublicationStateInput!]) {
	createBatchChange(batchSpec: $batchSpec, publicationStates: $publicationStates) { id url }
}`;

export type ApplyResult = Partial<ApplyBatchChangeFromUiMutation & CreateBatchChangeFromUiMutation>;

/**
 * The apply call for a spec: `applyBatchChange` with `ensureBatchChange` when it updates an
 * existing bulk change (so a bulk change created meanwhile is not overwritten), else
 * `createBatchChange` (which fails if one appeared meanwhile).
 */
export function applyMutation(
	spec: Pick<BatchSpecForApply, 'id' | 'appliesToBatchChange'>,
	publicationStates: PublicationStateInput[]
): { query: string; variables: Record<string, unknown>; field: 'applyBatchChange' | 'createBatchChange' } {
	const states = publicationStates.length > 0 ? publicationStates : null;
	if (spec.appliesToBatchChange) {
		return {
			query: APPLY_BATCH_CHANGE,
			variables: { batchSpec: spec.id, ensureBatchChange: spec.appliesToBatchChange.id, publicationStates: states },
			field: 'applyBatchChange'
		};
	}
	return {
		query: CREATE_BATCH_CHANGE,
		variables: { batchSpec: spec.id, publicationStates: states },
		field: 'createBatchChange'
	};
}
