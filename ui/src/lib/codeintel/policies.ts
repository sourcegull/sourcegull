/**
 * Code intelligence configuration policies: which branches, tags and commits keep
 * their precise indexes, and for how long. Site admins manage them on the site admin page
 * (all policies) and on a repository's code graph page (that repository's policies).
 */
import type {
	CodeIntelPoliciesQuery,
	GitObjectType,
	PreciseIndexRetentionQuery,
	PreviewGitObjectsQuery,
	PreviewRepositoriesQuery
} from '#lib/graphql-types.ts';

export type PolicyType = 'GIT_TREE' | 'GIT_TAG' | 'GIT_COMMIT';

/** A policy's type as the editor offers it (policies target branches, tags or commits). */
export function policyType(type: GitObjectType): PolicyType {
	return type === 'GIT_TAG' || type === 'GIT_COMMIT' ? type : 'GIT_TREE';
}


const POLICY_FIELDS = `id name type pattern protected repository { name url } repositoryPatterns
	retentionEnabled retentionDurationHours retainIntermediateCommits
	indexingEnabled indexCommitMaxAgeHours indexIntermediateCommits managedBy`;

export const POLICIES = `query CodeIntelPolicies($repo: ID, $query: String, $first: Int, $after: String) {
	codeIntelligenceConfigurationPolicies(repository: $repo, query: $query, first: $first, after: $after) {
		totalCount
		pageInfo { hasNextPage endCursor }
		nodes { ${POLICY_FIELDS} }
	}
}`;

export type PoliciesResult = CodeIntelPoliciesQuery;
export type PolicyNode = PoliciesResult['codeIntelligenceConfigurationPolicies']['nodes'][number];

export interface PoliciesVariables {
	repo: string | null;
	query: string | null;
	first: number;
	after: string | null;
}

/** The fields create and update take. */
export interface PolicyInput {
	repositoryPatterns: string[] | null;
	name: string;
	type: PolicyType;
	pattern: string;
	retentionEnabled: boolean;
	retentionDurationHours: number | null;
	retainIntermediateCommits: boolean;
	indexingEnabled: boolean;
	indexCommitMaxAgeHours: number | null;
	indexIntermediateCommits: boolean;
}

export type CreatePolicyVariables = PolicyInput & { repository: string | null };
export type UpdatePolicyVariables = PolicyInput & { id: string };

export const CREATE_POLICY = `mutation CreateCodeIntelPolicy($repository: ID, $repositoryPatterns: [String!],
		$name: String!, $type: GitObjectType!, $pattern: String!, $retentionEnabled: Boolean!,
		$retentionDurationHours: Int, $retainIntermediateCommits: Boolean!, $indexingEnabled: Boolean!,
		$indexCommitMaxAgeHours: Int, $indexIntermediateCommits: Boolean!) {
	createCodeIntelligenceConfigurationPolicy(repository: $repository, repositoryPatterns: $repositoryPatterns,
			name: $name, type: $type, pattern: $pattern, retentionEnabled: $retentionEnabled,
			retentionDurationHours: $retentionDurationHours, retainIntermediateCommits: $retainIntermediateCommits,
			indexingEnabled: $indexingEnabled, indexCommitMaxAgeHours: $indexCommitMaxAgeHours,
			indexIntermediateCommits: $indexIntermediateCommits) { id }
}`;

export const UPDATE_POLICY = `mutation UpdateCodeIntelPolicy($id: ID!, $repositoryPatterns: [String!],
		$name: String!, $type: GitObjectType!, $pattern: String!, $retentionEnabled: Boolean!,
		$retentionDurationHours: Int, $retainIntermediateCommits: Boolean!, $indexingEnabled: Boolean!,
		$indexCommitMaxAgeHours: Int, $indexIntermediateCommits: Boolean!) {
	updateCodeIntelligenceConfigurationPolicy(id: $id, repositoryPatterns: $repositoryPatterns,
			name: $name, type: $type, pattern: $pattern, retentionEnabled: $retentionEnabled,
			retentionDurationHours: $retentionDurationHours, retainIntermediateCommits: $retainIntermediateCommits,
			indexingEnabled: $indexingEnabled, indexCommitMaxAgeHours: $indexCommitMaxAgeHours,
			indexIntermediateCommits: $indexIntermediateCommits) { alwaysNil }
}`;

export const DELETE_POLICY = `mutation DeleteCodeIntelPolicy($id: ID!) {
	deleteCodeIntelligenceConfigurationPolicy(policy: $id) { alwaysNil }
}`;

export const PREVIEW_GIT_OBJECTS = `query PreviewGitObjects($repo: ID!, $type: GitObjectType!, $pattern: String!) {
	node(id: $repo) {
		... on Repository {
			previewGitObjectFilter(type: $type, pattern: $pattern, first: 10) {
				totalCount
				nodes { name rev committedAt }
			}
		}
	}
}`;

export interface PreviewGitObjectsVariables {
	repo: string;
	type: PolicyType;
	pattern: string;
}

export type PreviewGitObjectsResult = PreviewGitObjectsQuery;

export const PREVIEW_REPOSITORIES = `query PreviewRepositories($patterns: [String!]!) {
	previewRepositoryFilter(patterns: $patterns, first: 10) {
		totalMatches matchesAllRepos
		nodes { name url }
	}
}`;

export interface PreviewRepositoriesVariables {
	patterns: string[];
}

export type PreviewRepositoriesResult = PreviewRepositoriesQuery;

export const RETENTION_OVERVIEW = `query PreciseIndexRetention($id: ID!) {
	node(id: $id) {
		... on PreciseIndex {
			expired
			retentionPolicyOverview(matchesOnly: false, first: 100) {
				nodes {
					configurationPolicy { id name type pattern retentionDurationHours }
					matches
					protectingCommits
				}
			}
		}
	}
	dependents: preciseIndexes(dependentOf: $id, first: 10) {
		totalCount
		nodes { id inputRoot projectRoot { url repository { name } } }
	}
}`;

export type RetentionResult = PreciseIndexRetentionQuery;

/** What a policy's pattern names, in words. */
export function describeTarget(type: GitObjectType, pattern: string): string {
	switch (type) {
		case 'GIT_TREE':
			return pattern === '*' ? 'All branches' : `Branches matching ${pattern}`;
		case 'GIT_TAG':
			return pattern === '*' ? 'All tags' : `Tags matching ${pattern}`;
		case 'GIT_COMMIT':
			return `Commit ${pattern.length === 40 ? pattern.slice(0, 7) : pattern}`;
		default:
			return pattern;
	}
}

/** A duration in hours, in the largest whole unit (`null` is forever). */
export function formatHours(hours: number | null): string {
	if (hours === null) return 'forever';
	const unit = (n: number, word: string) => `${n} ${word}${n === 1 ? '' : 's'}`;
	if (hours % (24 * 7) === 0) return unit(hours / (24 * 7), 'week');
	if (hours % 24 === 0) return unit(hours / 24, 'day');
	return unit(hours, 'hour');
}

/** The repositories a policy applies to, in words. */
export function describeScope(p: Pick<PolicyNode, 'repository' | 'repositoryPatterns'>): string {
	if (p.repository) return p.repository.name;
	if (p.repositoryPatterns?.length) return p.repositoryPatterns.join(', ');
	return 'All repositories';
}

/** The retention a policy gives, in words (empty when retention is off). */
export function describeRetention(p: Pick<PolicyNode, 'type' | 'retentionEnabled' | 'retentionDurationHours' | 'retainIntermediateCommits'>): string {
	if (!p.retentionEnabled) return '';
	const kept = `Kept ${formatHours(p.retentionDurationHours)}`;
	return p.type === 'GIT_TREE' && p.retainIntermediateCommits ? `${kept}, every commit on the branch` : kept;
}

/** Duration units the editor offers. */
export const UNITS = [
	{ label: 'hours', hours: 1 },
	{ label: 'days', hours: 24 },
	{ label: 'weeks', hours: 24 * 7 }
] as const;

/** Splits hours into the largest unit that divides them, for the editor. */
export function splitHours(hours: number | null): { amount: number | null; unit: number } {
	if (hours === null) return { amount: null, unit: 24 };
	for (const u of [...UNITS].reverse()) {
		if (hours % u.hours === 0) return { amount: hours / u.hours, unit: u.hours };
	}
	return { amount: hours, unit: 1 };
}

/** Parses the editor's repository patterns (one per line or comma-separated); `null` for none. */
export function parsePatterns(text: string): string[] | null {
	const list = text
		.split(/[\n,]/)
		.map((s) => s.trim())
		.filter((s) => s !== '');
	return list.length ? list : null;
}
