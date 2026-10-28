/**
 * Local usage events: `logEvent` writes `event_logs` on this instance only.
 * The file page logs `ViewBlob`, which the recent views ownership signal counts; other
 * pages log views and clicks for the admin analytics.
 */
import { graphql } from '#lib/graphql.ts';

export const LOG_EVENT = `mutation LogEvent($event: String!, $userCookieID: String!, $url: String!, $argument: String) {
	logEvent(event: $event, userCookieID: $userCookieID, url: $url, source: WEB, argument: $argument) { alwaysNil }
}`;

/** Click and interaction events the admin analytics count (names match the server's). */
export const EVENTS = {
	/** A link to the repository, file or commit on its code host was clicked. */
	goToCodeHost: 'GoToCodeHostClicked',
	/** Code was copied from a file or a search result (a copy button or a text selection). */
	codeCopied: 'CodeCopied',
	/** A notebook query block was run. */
	notebookBlockRun: 'SearchNotebookBlockRun',
	/** The pointer entered a trend chart. */
	insightHover: 'InsightHover',
	/** A data point of a trend chart was clicked to open its search. */
	insightDataPointClick: 'InsightDataPointClick'
} as const;

const COOKIE_KEY = 'sourcegull.anonymousUserID';
/** The cookie the server reads for its own events of signed-out visitors. */
export const ANONYMOUS_UID_COOKIE = 'sourcegullAnonymousUid';

/** The `document.cookie` assignment that keeps `id` for a year. */
export function anonymousCookie(id: string, secure = false): string {
	return `${ANONYMOUS_UID_COOKIE}=${encodeURIComponent(id)}; path=/; max-age=31536000; SameSite=Lax${secure ? '; Secure' : ''}`;
}

/** A random id per browser, kept in localStorage and a cookie when available. */
export function anonymousUserID(): string {
	try {
		let id = localStorage.getItem(COOKIE_KEY);
		if (!id) {
			id = crypto.randomUUID();
			localStorage.setItem(COOKIE_KEY, id);
		}
		if (typeof document !== 'undefined' && !document.cookie.includes(`${ANONYMOUS_UID_COOKIE}=`)) {
			document.cookie = anonymousCookie(id, location.protocol === 'https:');
		}
		return id;
	} catch {
		return 'anonymous';
	}
}

/** Logs an event; failures are ignored (logging never breaks a page). */
export function logEvent(event: string, argument: Record<string, unknown> = {}): void {
	const url = typeof location === 'undefined' ? '' : location.href;
	void graphql<unknown, Record<string, unknown>>(LOG_EVENT, {
		event,
		userCookieID: anonymousUserID(),
		url,
		argument: JSON.stringify(argument)
	}).catch(() => undefined);
}

/** The argument of a `ViewBlob` event. */
export function viewBlobArgument(repoName: string, filePath: string): { repoName: string; filePath: string } {
	return { repoName, filePath: filePath.replace(/^\/+|\/+$/g, '') };
}

