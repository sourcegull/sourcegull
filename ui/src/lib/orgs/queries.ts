/**
 * GraphQL operations of the organization, team and settings pages:
 * organizations with members and invitations, the settings cascade editor, and teams with their
 * hierarchy and the GitLab group sync.
 */
import type {
	CreateOrganizationMutation,
	CreateTeamMutation,
	DefaultSettingsQuery,
	GlobalSettingsQuery,
	InvitationByTokenQuery,
	InviteUserToOrganizationMutation,
	OrganizationMembersQuery,
	OrganizationQuery,
	OrganizationSettingsQuery,
	OrganizationsQuery,
	PendingInvitationsQuery,
	SyncTeamsFromGitLabGroupsMutation,
	TeamQuery,
	TeamsQuery,
	ViewerOrganizationsQuery
} from '#lib/graphql-types.ts';

/** Variables of the operations below. */
export type Vars = Record<string, unknown>;

// ---------------------------------------------------------------- organizations

export interface OrgSummary {
	id: string;
	name: string;
	displayName: string | null;
	url: string;
	createdAt: string;
}

export const ORGANIZATIONS = `query Organizations($first: Int, $query: String) {
	organizations(first: $first, query: $query) {
		totalCount
		nodes { id name displayName url createdAt }
	}
}`;
export type OrganizationsResult = OrganizationsQuery;

export const CREATE_ORGANIZATION = `mutation CreateOrganization($name: String!, $displayName: String) {
	createOrganization(name: $name, displayName: $displayName) { id name url }
}`;
export type CreateOrganizationResult = CreateOrganizationMutation;

export const DELETE_ORGANIZATION = `mutation DeleteOrganization($organization: ID!) {
	deleteOrganization(organization: $organization) { alwaysNil }
}`;

export const UPDATE_ORGANIZATION = `mutation UpdateOrganization($id: ID!, $displayName: String) {
	updateOrganization(id: $id, displayName: $displayName) { id displayName }
}`;

export interface OrgHeader extends OrgSummary {
	viewerIsMember: boolean;
	viewerCanAdminister: boolean;
	viewerPendingInvitation: {
		id: string;
		sender: { username: string } | null;
	} | null;
}

export const ORGANIZATION = `query Organization($name: String!) {
	organization(name: $name) {
		id name displayName url createdAt viewerIsMember viewerCanAdminister
		viewerPendingInvitation { id sender { username } }
	}
}`;
export type OrganizationResult = OrganizationQuery;

export interface MemberNode {
	id: string;
	username: string;
	displayName: string | null;
	avatarURL: string | null;
}

export const ORGANIZATION_MEMBERS = `query OrganizationMembers($name: String!, $first: Int, $after: String, $query: String) {
	organization(name: $name) {
		id
		members(first: $first, after: $after, query: $query) {
			totalCount
			pageInfo { hasNextPage endCursor }
			nodes { id username displayName avatarURL }
		}
	}
}`;
export type OrganizationMembersResult = OrganizationMembersQuery;

export const ADD_USER_TO_ORGANIZATION = `mutation AddUserToOrganization($organization: ID!, $username: String!) {
	addUserToOrganization(organization: $organization, username: $username) { alwaysNil }
}`;

export const REMOVE_USER_FROM_ORGANIZATION = `mutation RemoveUserFromOrganization($user: ID!, $organization: ID!) {
	removeUserFromOrganization(user: $user, organization: $organization) { alwaysNil }
}`;

// ---------------------------------------------------------------- invitations

export interface InvitationNode {
	id: string;
	recipient: { username: string } | null;
	recipientEmail: string | null;
	sender: { username: string } | null;
	createdAt: string;
	notifiedAt: string | null;
	expiresAt: string | null;
}

export const INVITE_USER_TO_ORGANIZATION = `mutation InviteUserToOrganization($organization: ID!, $username: String, $email: String) {
	inviteUserToOrganization(organization: $organization, username: $username, email: $email) {
		sentInvitationEmail
		invitationURL
	}
}`;
export type InviteUserToOrganizationResult = InviteUserToOrganizationMutation;

export const PENDING_INVITATIONS = `query PendingInvitations($org: ID!) {
	pendingInvitations(org: $org) {
		id recipient { username } recipientEmail sender { username } createdAt notifiedAt expiresAt
	}
}`;
export type PendingInvitationsResult = PendingInvitationsQuery;

export const REVOKE_ORGANIZATION_INVITATION = `mutation RevokeOrganizationInvitation($id: ID!) {
	revokeOrganizationInvitation(organizationInvitation: $id) { alwaysNil }
}`;

export const RESEND_ORGANIZATION_INVITATION = `mutation ResendOrganizationInvitation($id: ID!) {
	resendOrganizationInvitationNotification(organizationInvitation: $id) { alwaysNil }
}`;

export type InvitationResponse = 'ACCEPT' | 'REJECT';

export const RESPOND_TO_ORGANIZATION_INVITATION = `mutation RespondToOrganizationInvitation($id: ID!, $response: OrganizationInvitationResponseType!) {
	respondToOrganizationInvitation(organizationInvitation: $id, responseType: $response) { alwaysNil }
}`;

export interface ViewerInvitation {
	id: string;
	organization: { name: string; displayName: string | null };
	sender: { username: string } | null;
	recipientEmail: string | null;
	isVerifiedEmail: boolean | null;
	expiresAt: string | null;
}

export const VIEWER_ORGANIZATIONS = `query ViewerOrganizations {
	currentUser { id organizations { totalCount nodes { id name displayName url createdAt } } }
	viewerOrganizationInvitations {
		id organization { name displayName } sender { username } recipientEmail isVerifiedEmail expiresAt
	}
}`;
export type ViewerOrganizationsResult = ViewerOrganizationsQuery;

export const INVITATION_BY_TOKEN = `query InvitationByToken($token: String!) {
	invitationByToken(token: $token) {
		id organization { name displayName url } sender { username } recipientEmail isVerifiedEmail
		expiresAt respondedAt revokedAt
	}
}`;
export type InvitationByTokenResult = InvitationByTokenQuery;

/** Whether `input` looks like an email address (invite by email) rather than a username. */
export function isEmailAddress(input: string): boolean {
	return /^[^\s@]+@[^\s@]+$/.test(input.trim());
}

// ---------------------------------------------------------------- settings

export interface SettingsVersion {
	id: number;
	contents: string;
	createdAt: string;
	author: { username: string } | null;
}

export const ORGANIZATION_SETTINGS = `query OrganizationSettings($name: String!) {
	organization(name: $name) {
		id displayName
		latestSettings { id contents createdAt author { username } }
		settingsCascade { final }
	}
}`;
export type OrganizationSettingsResult = OrganizationSettingsQuery;

/**
 * The global settings; `readOnly` (and `viewerCanAdminister` false for a site admin) when they come
 * from SOURCEGULL_GLOBAL_SETTINGS_FILE, whose path `declaredConfigFiles` gives.
 */
export const GLOBAL_SETTINGS = `query GlobalSettings {
	site {
		id
		viewerCanAdminister
		latestSettings { id contents createdAt author { username } readOnly }
		declaredConfigFiles { source path }
	}
}`;
export type GlobalSettingsResult = GlobalSettingsQuery;

export const OVERWRITE_SETTINGS = `mutation OverwriteSettings($subject: ID!, $lastID: Int, $contents: String!) {
	settingsMutation(input: { subject: $subject, lastID: $lastID }) {
		overwriteSettings(contents: $contents) { empty { alwaysNil } }
	}
}`;

/**
 * The document a settings editor starts from when its subject has no settings yet: every setting
 * commented out with its default and description, grouped by section.
 */
export const DEFAULT_SETTINGS = `query DefaultSettings {
	site { defaultSettings }
}`;
export type DefaultSettingsResult = DefaultSettingsQuery;

/** The text shown in the editor for a subject without settings when the template can't load. */
export const EMPTY_SETTINGS = '{\n\t// Settings for this subject; see the settings reference at /help/settings.\n}\n';

/** Pretty-prints the merged settings (`SettingsCascade.final`) for display. */
export function formatFinalSettings(final: string): string {
	try {
		return JSON.stringify(JSON.parse(final), null, 2);
	} catch {
		return final;
	}
}

// ---------------------------------------------------------------- teams

export interface TeamSummary {
	id: string;
	name: string;
	displayName: string | null;
	url: string;
	readonly: boolean;
	syncedFromCodeHost: boolean;
	members: { totalCount: number };
	childTeams: { totalCount: number };
}

const TEAM_SUMMARY = `id name displayName url readonly syncedFromCodeHost members { totalCount } childTeams { totalCount }`;

export const TEAMS = `query Teams($first: Int, $after: String, $search: String, $includeChildTeams: Boolean) {
	teams(first: $first, after: $after, search: $search, includeChildTeams: $includeChildTeams) {
		totalCount
		pageInfo { hasNextPage endCursor }
		nodes { ${TEAM_SUMMARY} }
	}
}`;
export type TeamsResult = TeamsQuery;

export const TEAM = `query Team($name: String!) {
	team(name: $name) {
		id name displayName url readonly syncedFromCodeHost
		viewerCanAdminister
		creator { username }
		parentTeam { id name displayName url }
		childTeams(first: 100) { totalCount nodes { ${TEAM_SUMMARY} } }
		members(first: 100) {
			totalCount
			nodes { ... on User { id username displayName avatarURL } }
		}
	}
}`;
export interface TeamDetail extends TeamSummary {
	viewerCanAdminister: boolean;
	creator: { username: string } | null;
	parentTeam: {
		id: string;
		name: string;
		displayName: string | null;
		url: string;
	} | null;
	childTeams: { totalCount: number; nodes: TeamSummary[] };
	members: { totalCount: number; nodes: MemberNode[] };
}
export type TeamResult = TeamQuery;

export const CREATE_TEAM = `mutation CreateTeam($name: String!, $displayName: String, $parentTeamName: String) {
	createTeam(name: $name, displayName: $displayName, parentTeamName: $parentTeamName) { id name url }
}`;
export type CreateTeamResult = CreateTeamMutation;

export const UPDATE_TEAM = `mutation UpdateTeam($id: ID!, $displayName: String, $parentTeamName: String, $makeRoot: Boolean) {
	updateTeam(id: $id, displayName: $displayName, parentTeamName: $parentTeamName, makeRoot: $makeRoot) {
		id
	}
}`;

export const DELETE_TEAM = `mutation DeleteTeam($id: ID!) {
	deleteTeam(id: $id) { alwaysNil }
}`;

export const ADD_TEAM_MEMBERS = `mutation AddTeamMembers($team: ID!, $members: [TeamMemberInput!]!) {
	addTeamMembers(team: $team, members: $members) { id }
}`;

export const REMOVE_TEAM_MEMBERS = `mutation RemoveTeamMembers($team: ID!, $members: [TeamMemberInput!]!) {
	removeTeamMembers(team: $team, members: $members) { id }
}`;

export const SYNC_TEAMS_FROM_GITLAB_GROUPS = `mutation SyncTeamsFromGitLabGroups {
	syncTeamsFromGitLabGroups { created updated deleted skipped members warnings }
}`;
export type SyncTeamsResult = SyncTeamsFromGitLabGroupsMutation;

/** The URL of a team page (team names may contain `/`, kept as path segments). */
export function teamUrl(name: string): string {
	return '/teams/' + name.split('/').map(encodeURIComponent).join('/');
}

/** Splits a list of usernames typed by the user ("alice, bob\ncarol") into unique names. */
export function parseUsernames(input: string): string[] {
	return [
		...new Set(
			input
				.split(/[\s,]+/)
				.map((s) => s.trim().replace(/^@/, ''))
				.filter(Boolean)
		)
	];
}
