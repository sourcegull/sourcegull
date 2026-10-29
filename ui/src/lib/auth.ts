/**
 * Builtin username/password authentication against the `/-/` endpoints, plus the routing rules that
 * decide where an anonymous or not-yet-initialised visitor is sent.
 */
import { REQUESTED_WITH_HEADER, REQUESTED_WITH_VALUE } from './graphql.ts';

/** The signed-in user as reported by `GET /-/auth-state`. */
export interface AuthUser {
	id: string;
	username: string;
	siteAdmin: boolean;
}

/** An external sign-in option (e.g. GitLab, GitHub or Gitea OAuth) from `auth.providers`. */
export interface AuthProviderButton {
	type: string;
	displayName: string;
	/** Starts the flow; append `&returnTo=` to come back to a page. */
	authenticationURL: string;
}

/** Response of `GET /-/auth-state`. */
export interface AuthState {
	authenticated: boolean;
	user?: AuthUser;
	allowSignup: boolean;
	accessRequest?: boolean;
	/** No users exist yet: the first sign-up initialises the site and becomes site admin. */
	needsSiteInit: boolean;
	/** Password sign-in is enabled (absent: assume it is). */
	builtin?: boolean;
	/** External sign-in options. */
	providers?: AuthProviderButton[];
	/** The signed-in viewer may use bulk changes (`batchChanges.enabled`, `restrictToAdmins`). */
	batchChangesEnabled?: boolean;
}

/** Thrown by {@link postAuth} when the server answers `{ error }` or a non-2xx status. */
export class AuthError extends Error {
	readonly status: number;
	constructor(message: string, status: number) {
		super(message);
		this.name = 'AuthError';
		this.status = status;
	}
}

/** Fetches the current authentication state. */
export async function fetchAuthState(doFetch: typeof fetch = fetch): Promise<AuthState> {
	const res = await doFetch('/-/auth-state', {
		credentials: 'same-origin',
		headers: { Accept: 'application/json' }
	});
	if (!res.ok) throw new AuthError(`Could not load auth state (${res.status})`, res.status);
	const body = (await res.json()) as Partial<AuthState>;
	return {
		authenticated: body.authenticated === true,
		user: body.authenticated ? body.user : undefined,
		allowSignup: body.allowSignup === true,
		accessRequest: body.accessRequest === true,
		needsSiteInit: body.needsSiteInit === true,
		builtin: body.builtin !== false,
		providers: Array.isArray(body.providers) ? body.providers : [],
		batchChangesEnabled: body.authenticated === true && body.batchChangesEnabled === true
	};
}

/** The URL that starts an external sign-in and comes back to `returnTo`. */
export function providerSignInURL(p: AuthProviderButton, returnTo: string): string {
	const sep = p.authenticationURL.includes('?') ? '&' : '?';
	return `${p.authenticationURL}${sep}returnTo=${encodeURIComponent(returnTo)}`;
}

/**
 * The sign-in page lists external providers first and keeps the password form behind a toggle
 * when both are available. Returns whether that form starts collapsed: `?local=1` opens it.
 */
export function builtinFormCollapsed(
	state: AuthState,
	url: { searchParams: { get(name: string): string | null } }
): boolean {
	if (state.builtin === false || (state.providers ?? []).length === 0) return false;
	return url.searchParams.get('local') !== '1';
}

/** The logo shown on a provider's sign-in button. */
export function providerLogo(p: AuthProviderButton): 'gitlab' | 'github' | 'gitea' | 'key' {
	return p.type === 'gitlab' || p.type === 'github' || p.type === 'gitea' ? p.type : 'key';
}

/** POSTs a JSON body to one of the `/-/sign-*` endpoints. */
export async function postAuth(
	endpoint: 'sign-in' | 'sign-up' | 'sign-out',
	body: Record<string, string> = {},
	doFetch: typeof fetch = fetch
): Promise<void> {
	return postJSON(`/-/${endpoint}`, body, doFetch);
}

/** POSTs a JSON body to an auth endpoint, throwing [`AuthError`] with the server's message. */
async function postJSON(
	path: string,
	body: Record<string, string>,
	doFetch: typeof fetch
): Promise<void> {
	let res: Response;
	try {
		res = await doFetch(path, {
			method: 'POST',
			credentials: 'same-origin',
			headers: {
				'Content-Type': 'application/json',
				Accept: 'application/json',
				[REQUESTED_WITH_HEADER]: REQUESTED_WITH_VALUE
			},
			body: JSON.stringify(body)
		});
	} catch (err) {
		throw new AuthError(`Network error: ${String(err)}`, 0);
	}
	let json: { ok?: boolean; error?: string } = {};
	try {
		json = (await res.json()) as typeof json;
	} catch {
		// Non-JSON body: fall through to status handling.
	}
	if (!res.ok || json.error) {
		throw new AuthError(json.error ?? `Request failed (${res.status})`, res.status);
	}
}

export const signIn = (username: string, password: string, f?: typeof fetch) =>
	postAuth('sign-in', { username, password }, f);
export const signUp = (username: string, email: string, password: string, f?: typeof fetch) =>
	postAuth('sign-up', { username, email, password }, f);
export const signOut = (f?: typeof fetch) => postAuth('sign-out', {}, f);
/** Submits an access request (`auth.accessRequest`). */
export const requestAccess = (
	name: string,
	email: string,
	additionalInfo: string,
	f: typeof fetch = fetch
) => postJSON('/.api/request-access', { name, email, additionalInfo }, f);
/** Sets the first password with an approved access request's setup link. */
export const setInvitePassword = (token: string, password: string, f: typeof fetch = fetch) =>
	postJSON('/.api/set-invite-password', { token, password }, f);

/** Asks for a password reset link (the answer is the same for unknown emails). */
export const requestPasswordReset = (email: string, f: typeof fetch = fetch) =>
	postJSON('/-/reset-password-init', { email }, f);
/** Sets a new password with a reset link's user ID and code. */
export const resetPassword = (
	userID: string,
	code: string,
	password: string,
	f: typeof fetch = fetch
) => postJSON('/-/reset-password-code', { userID, code, password }, f);

/** Routes that anonymous visitors may open. */
const PUBLIC_ROUTES = [
	'/sign-in',
	'/sign-up',
	'/request-access',
	'/set-password',
	'/forgot-password',
	'/reset-password'
];

function isPublic(pathname: string): boolean {
	return PUBLIC_ROUTES.some((p) => pathname === p || pathname.startsWith(`${p}/`));
}

/**
 * Sanitises a `returnTo` parameter so it can only point inside this app (no open redirects),
 * defaulting to `/search`.
 */
export function safeReturnTo(raw: string | null | undefined): string {
	if (!raw || !raw.startsWith('/') || raw.startsWith('//') || raw.startsWith('/\\')) return '/search';
	if (isPublic(raw.split(/[?#]/)[0] ?? raw)) return '/search';
	return raw;
}

/**
 * Decides whether a navigation to `url` must be redirected, given the auth state.
 * Returns the redirect target, or `null` to let the navigation proceed.
 */
export function authRedirect(state: AuthState, url: URL): string | null {
	const path = url.pathname;
	if (state.needsSiteInit) {
		return path === '/sign-up' ? null : '/sign-up';
	}
	if (state.authenticated) {
		// Reset and invite links work while signed in (completing a reset signs every session
		// out), and a sign-in error (a failed provider sign-in or email verification) stays shown.
		if (path === '/reset-password' || path === '/set-password') return null;
		if (path === '/sign-in' && url.searchParams.has('error')) return null;
		return isPublic(path) ? safeReturnTo(url.searchParams.get('returnTo')) : null;
	}
	if (path === '/sign-up' && !state.allowSignup) return '/sign-in';
	if (path === '/request-access' && !state.accessRequest) return '/sign-in';
	if (isPublic(path)) return null;
	const returnTo = path + url.search;
	return returnTo === '/' ? '/sign-in' : `/sign-in?returnTo=${encodeURIComponent(returnTo)}`;
}

/** Whether a page that requires site admin may be shown. */
export function canAccessSiteAdmin(state: AuthState): boolean {
	return state.authenticated && state.user?.siteAdmin === true;
}

let cached: Promise<AuthState> | undefined;

/**
 * Returns the auth state, fetching it once and reusing it across navigations.
 * Call {@link resetAuthState} after signing in or out.
 */
export function loadAuthState(doFetch: typeof fetch = fetch): Promise<AuthState> {
	cached ??= fetchAuthState(doFetch).catch((err: unknown) => {
		cached = undefined;
		throw err;
	});
	return cached;
}

/** Forgets the cached auth state so the next {@link loadAuthState} refetches it. */
export function resetAuthState(): void {
	cached = undefined;
}
