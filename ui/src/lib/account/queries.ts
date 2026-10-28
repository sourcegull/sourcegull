/**
 * GraphQL operations of the account settings pages (profile, password, emails) and of the site
 * admin user actions (deactivate, reactivate).
 */
import type {
	AccountEmailsQuery,
	AccountProfileQuery,
	AccountPasswordQuery,
	UpdateProfileMutation
} from '#lib/graphql-types.ts';

export const ACCOUNT_PROFILE = `query AccountProfile {
	currentUser { id username displayName avatarURL viewerCanChangeUsername scimControlled }
}`;
export type AccountProfileResult = AccountProfileQuery;

export const UPDATE_PROFILE = `mutation UpdateProfile($user: ID!, $username: String, $displayName: String, $avatarURL: String) {
	updateUser(user: $user, username: $username, displayName: $displayName, avatarURL: $avatarURL) {
		id username displayName avatarURL
	}
}`;
export type UpdateProfileResult = UpdateProfileMutation;

export const ACCOUNT_PASSWORD = `query AccountPassword {
	currentUser { id hasPassword }
}`;
export type AccountPasswordResult = AccountPasswordQuery;

export const UPDATE_PASSWORD = `mutation UpdatePassword($oldPassword: String, $newPassword: String!) {
	updatePassword(oldPassword: $oldPassword, newPassword: $newPassword) { alwaysNil }
}`;

export const ACCOUNT_EMAILS = `query AccountEmails {
	currentUser { id scimControlled emails { email verified isPrimary } }
}`;
export type AccountEmailsResult = AccountEmailsQuery;

export const ADD_USER_EMAIL = `mutation AddUserEmail($user: ID!, $email: String!) {
	addUserEmail(user: $user, email: $email) { alwaysNil }
}`;
export const REMOVE_USER_EMAIL = `mutation RemoveUserEmail($user: ID!, $email: String!) {
	removeUserEmail(user: $user, email: $email) { alwaysNil }
}`;
export const SET_USER_EMAIL_PRIMARY = `mutation SetUserEmailPrimary($user: ID!, $email: String!) {
	setUserEmailPrimary(user: $user, email: $email) { alwaysNil }
}`;
export const RESEND_VERIFICATION_EMAIL = `mutation ResendVerificationEmail($user: ID!, $email: String!) {
	resendVerificationEmail(user: $user, email: $email) { alwaysNil }
}`;

export const DEACTIVATE_USER = `mutation DeactivateUser($user: ID!) {
	deactivateUser(user: $user) { alwaysNil }
}`;
export const REACTIVATE_USER = `mutation ReactivateUser($user: ID!) {
	reactivateUser(user: $user) { id }
}`;
