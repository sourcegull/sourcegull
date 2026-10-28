/**
 * The prompt shown to a signed-in user whose code host token cannot be used by the
 * permissions sync (rejected, or without the scope that lists private repositories): sign in
 * again with that code host.
 */
import type { CodeHostSignInsNeededQuery } from '#lib/graphql-types.ts';

export const CODE_HOST_SIGN_INS_NEEDED = `query CodeHostSignInsNeeded {
	currentUser {
		codeHostSignInsNeeded {
			serviceType
			serviceID
			displayName
			reason
			authenticationURL
		}
	}
}`;

export type CodeHostSignIn = NonNullable<
	CodeHostSignInsNeededQuery['currentUser']
>['codeHostSignInsNeeded'][number];

export interface SignInPrompt {
	key: string;
	message: string;
	/** The sign-in link (coming back to `returnTo`), or null when no sign-in provider matches. */
	href: string | null;
	linkText: string;
}

/** The prompts for `needed`, coming back to `returnTo` after signing in. */
export function signInPrompts(needed: CodeHostSignIn[], returnTo: string): SignInPrompt[] {
	return needed.map((n) => {
		const why =
			n.reason === 'MISSING_SCOPE'
				? `Sourcegull needs more access to your ${n.displayName} account`
				: `Your ${n.displayName} sign-in has expired`;
		const sep = n.authenticationURL?.includes('?') ? '&' : '?';
		return {
			key: `${n.serviceType} ${n.serviceID}`,
			message: `${why}. Sign in again with ${n.displayName} to see your private repositories.`,
			href: n.authenticationURL ? `${n.authenticationURL}${sep}returnTo=${encodeURIComponent(returnTo)}` : null,
			linkText: `Sign in with ${n.displayName}`
		};
	});
}
