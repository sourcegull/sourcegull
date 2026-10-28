/**
 * GraphQL operations of Code Ownership: the ownership panel of file and
 * directory pages (owners, signals and coverage), assignments, and the site admin pages for
 * ingested CODEOWNERS files, signal configuration and ownership analytics.
 */
import { graphql } from '#lib/graphql.ts';
import type {
	BlobOwnershipQuery,
	CodeownersFilesQuery,
	OwnershipAnalyticsQuery,
	OwnTeamByNameQuery,
	OwnUserByNameQuery,
	TreeOwnershipQuery
} from '#lib/graphql-types.ts';

type Vars = Record<string, unknown>;

const OWNERSHIP_FIELDS = `
	totalCount
	totalOwners
	nodes {
		owner {
			__typename
			... on Person { name email displayName avatarURL user { id username displayName url } }
			... on Team { id name teamDisplayName: displayName url external }
		}
		reasons {
			__typename
			... on CodeownersFileEntry {
				ruleLineMatch section pattern
				codeownersFile { __typename path url }
			}
			... on AssignedOwner { isDirectMatch }
			... on RecentContributorOwnershipSignal { contributionCount }
			... on RecentViewOwnershipSignal { viewCount }
		}
	}`;

const STATS_FIELDS = `totalFiles totalCodeownedFiles totalOwnedFiles totalAssignedOwnershipFiles updatedAt`;

export const BLOB_OWNERSHIP = `query BlobOwnership($repo: String!, $rev: String!, $path: String!) {
	repository(name: $repo) {
		id
		commit(rev: $rev) { blob(path: $path) { ownership(first: 100) { ${OWNERSHIP_FIELDS} } } }
	}
	currentUser { id permissions { nodes { namespace action } } }
}`;

export const TREE_OWNERSHIP = `query TreeOwnership($repo: String!, $rev: String!, $path: String!) {
	repository(name: $repo) {
		id
		commit(rev: $rev) {
			tree(path: $path) { ownership(first: 100) { ${OWNERSHIP_FIELDS} } ownershipStats { ${STATS_FIELDS} } }
		}
	}
	currentUser { id permissions { nodes { namespace action } } }
}`;

export interface OwnerPerson {
	__typename: 'Person';
	name: string;
	email: string;
	displayName: string;
	avatarURL: string | null;
	user: { id: string; username: string; displayName: string | null; url: string } | null;
}

export interface OwnerTeam {
	__typename: 'Team';
	id: string;
	name: string;
	/** `displayName`, aliased: it is nullable on Team but not on Person. */
	teamDisplayName: string | null;
	url: string;
	external: boolean;
}

export type OwnershipReason =
	| {
			__typename: 'CodeownersFileEntry';
			ruleLineMatch: number;
			section: string;
			pattern: string;
			codeownersFile: { __typename: 'GitBlob' | 'VirtualFile'; path: string; url: string };
	  }
	| { __typename: 'AssignedOwner'; isDirectMatch: boolean }
	| { __typename: 'RecentContributorOwnershipSignal'; contributionCount: number }
	| { __typename: 'RecentViewOwnershipSignal'; viewCount: number };

/** Ownership coverage of files on the default branch. */
export interface OwnershipStats {
	totalFiles: number;
	totalCodeownedFiles: number;
	totalOwnedFiles: number;
	totalAssignedOwnershipFiles: number;
	updatedAt: string | null;
}

export interface Ownership {
	owner: OwnerPerson | OwnerTeam;
	reasons: OwnershipReason[];
}

export interface OwnershipConnection {
	totalCount: number;
	totalOwners: number;
	nodes: Ownership[];
}

interface Viewer {
	id: string;
	permissions: { nodes: { namespace: string; action: string }[] };
}

export type BlobOwnershipResult = BlobOwnershipQuery;

export type TreeOwnershipResult = TreeOwnershipQuery;

export interface LoadedOwnership {
	repoID: string;
	ownership: OwnershipConnection | null;
	canAssign: boolean;
	/** Directories: coverage on the default branch. */
	stats: OwnershipStats | null;
}

/** Whether the viewer holds `OWNERSHIP#ASSIGN`. */
export function canAssign(viewer: Viewer | null): boolean {
	return !!viewer?.permissions.nodes.some((p) => p.namespace === 'OWNERSHIP' && p.action === 'ASSIGN');
}

/** Loads the owners of a file or directory and whether the viewer may assign owners. */
export async function loadOwnership(repo: string, rev: string, path: string, isDir: boolean): Promise<LoadedOwnership | null> {
	if (isDir) {
		const r = await graphql<TreeOwnershipResult, Vars>(TREE_OWNERSHIP, { repo, rev, path });
		if (!r.repository) return null;
		return {
			repoID: r.repository.id,
			ownership: r.repository.commit?.tree?.ownership ?? null,
			canAssign: canAssign(r.currentUser),
			stats: r.repository.commit?.tree?.ownershipStats ?? null
		};
	}
	const r = await graphql<BlobOwnershipResult, Vars>(BLOB_OWNERSHIP, { repo, rev, path });
	if (!r.repository) return null;
	return {
		repoID: r.repository.id,
		ownership: r.repository.commit?.blob?.ownership ?? null,
		canAssign: canAssign(r.currentUser),
		stats: null
	};
}

export const USER_BY_NAME = `query OwnUserByName($username: String!) { user(username: $username) { id } }`;
export const TEAM_BY_NAME = `query OwnTeamByName($name: String!) { team(name: $name) { id } }`;

export const ASSIGN_OWNER = `mutation AssignOwner($input: AssignOwnerOrTeamInput!) {
	assignOwner(input: $input) { alwaysNil }
}`;
export const REMOVE_ASSIGNED_OWNER = `mutation RemoveAssignedOwner($input: AssignOwnerOrTeamInput!) {
	removeAssignedOwner(input: $input) { alwaysNil }
}`;
export const ASSIGN_TEAM = `mutation AssignTeam($input: AssignOwnerOrTeamInput!) {
	assignTeam(input: $input) { alwaysNil }
}`;
export const REMOVE_ASSIGNED_TEAM = `mutation RemoveAssignedTeam($input: AssignOwnerOrTeamInput!) {
	removeAssignedTeam(input: $input) { alwaysNil }
}`;

export interface AssignInput {
	assignedOwnerID: string;
	repoID: string;
	absolutePath: string;
}

/**
 * Assigns a user (`@username` or `username`) or a team (`team:name`) to a path; returns an error
 * message when the name matches nobody.
 */
export async function assign(who: string, repoID: string, absolutePath: string): Promise<string | null> {
	const name = who.trim().replace(/^@/, '');
	if (!name) return 'Enter a username or team:name.';
	if (name.startsWith('team:')) {
		const teamName = name.slice('team:'.length);
		const r = await graphql<OwnTeamByNameQuery, Vars>(TEAM_BY_NAME, { name: teamName });
		if (!r.team) return `No team named ${teamName}.`;
		await graphql<unknown, Vars>(ASSIGN_TEAM, { input: { assignedOwnerID: r.team.id, repoID, absolutePath } });
		return null;
	}
	const r = await graphql<OwnUserByNameQuery, Vars>(USER_BY_NAME, { username: name });
	if (!r.user) return `No user named ${name}.`;
	await graphql<unknown, Vars>(ASSIGN_OWNER, { input: { assignedOwnerID: r.user.id, repoID, absolutePath } });
	return null;
}

/** Removes a direct assignment of the owner. */
export async function unassign(owner: OwnerPerson | OwnerTeam, repoID: string, absolutePath: string): Promise<void> {
	if (owner.__typename === 'Team') {
		await graphql<unknown, Vars>(REMOVE_ASSIGNED_TEAM, { input: { assignedOwnerID: owner.id, repoID, absolutePath } });
	} else if (owner.user) {
		await graphql<unknown, Vars>(REMOVE_ASSIGNED_OWNER, { input: { assignedOwnerID: owner.user.id, repoID, absolutePath } });
	}
}

/** A short label for a reason. */
export function reasonLabel(r: OwnershipReason): string {
	switch (r.__typename) {
		case 'CodeownersFileEntry':
			return `CODEOWNERS line ${r.ruleLineMatch}${r.section.toLowerCase() === 'codeowners' ? '' : ` [${r.section}]`}`;
		case 'AssignedOwner':
			return r.isDirectMatch ? 'Assigned' : 'Assigned to a parent directory';
		case 'RecentContributorOwnershipSignal':
			return `Recent contributor · ${plural(r.contributionCount, 'commit')}`;
		case 'RecentViewOwnershipSignal':
			return `Recent viewer · ${plural(r.viewCount, 'view')}`;
	}
}

function plural(n: number, word: string): string {
	return `${n} ${word}${n === 1 ? '' : 's'}`;
}

/** Whether a reason is a signal (recent contributor or viewer) rather than ownership. */
export function isSignal(r: OwnershipReason): boolean {
	return r.__typename === 'RecentContributorOwnershipSignal' || r.__typename === 'RecentViewOwnershipSignal';
}

/** `owned` as a percentage of `total` (one decimal), or null without files. */
export function coverage(owned: number, total: number): number | null {
	if (total <= 0) return null;
	return Math.round((owned / total) * 1000) / 10;
}

/** The display name of an owner. */
export function ownerName(o: OwnerPerson | OwnerTeam): string {
	if (o.__typename === 'Team') return o.teamDisplayName || o.name;
	if (o.user) return o.user.displayName || o.user.username;
	// A commit author (recent contributor) has a name and an email; a CODEOWNERS owner has a
	// handle or an email.
	if (o.email) return o.name && o.displayName ? o.displayName : o.email;
	return `@${o.name}`;
}

// ---- Site admin: ingested CODEOWNERS files ---------------------------------------------------

export const CODEOWNERS_FILES = `query CodeownersFiles($first: Int, $after: Int) {
	codeownersIngestedFiles(first: $first, after: $after) {
		totalCount
		pageInfo { hasNextPage endCursor }
		nodes { id contents createdAt updatedAt repository { id name url } }
	}
}`;

export interface CodeownersFile {
	id: string;
	contents: string;
	createdAt: string;
	updatedAt: string;
	repository: { id: string; name: string; url: string } | null;
}

export type CodeownersFilesResult = CodeownersFilesQuery;

export const ADD_CODEOWNERS_FILE = `mutation AddCodeownersFile($input: CodeownersFileInput!) {
	addCodeownersFile(input: $input) { id }
}`;
export const UPDATE_CODEOWNERS_FILE = `mutation UpdateCodeownersFile($input: CodeownersFileInput!) {
	updateCodeownersFile(input: $input) { id }
}`;
export const DELETE_CODEOWNERS_FILES = `mutation DeleteCodeownersFiles($repositories: [DeleteCodeownersFilesInput!]!) {
	deleteCodeownersFiles(repositories: $repositories) { alwaysNil }
}`;

// ---- Site admin: signals and analytics -------------------------------------------------------

export const OWN_SIGNAL_CONFIGURATIONS = `query OwnSignalConfigurations {
	ownSignalConfigurations { name description isEnabled excludedRepoPatterns }
}`;

export const UPDATE_OWN_SIGNAL_CONFIGURATIONS = `mutation UpdateOwnSignalConfigurations($input: UpdateSignalConfigurationsInput!) {
	updateOwnSignalConfigurations(input: $input) { name description isEnabled excludedRepoPatterns }
}`;

export interface OwnSignalConfiguration {
	name: string;
	description: string;
	isEnabled: boolean;
	excludedRepoPatterns: string[];
}

export const OWNERSHIP_ANALYTICS = `query OwnershipAnalytics($first: Int, $after: String) {
	instanceOwnershipStats { ${STATS_FIELDS} }
	ownershipStatsByRepository(first: $first, after: $after) {
		totalCount
		pageInfo { hasNextPage endCursor }
		nodes { name repository { id url } stats { ${STATS_FIELDS} } }
	}
	ownSignalConfigurations { name isEnabled }
}`;

export type OwnershipAnalyticsResult = OwnershipAnalyticsQuery;

/** Parses the excluded patterns field: one pattern per line or comma-separated. */
export function parsePatterns(text: string): string[] {
	return text
		.split(/[\n,]/)
		.map((p) => p.trim())
		.filter((p) => p.length > 0);
}
