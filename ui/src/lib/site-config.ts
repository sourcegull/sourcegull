/** Helpers for the site configuration editor. */
import { GraphQLRequestError } from './graphql.ts';

/** `extensions.code` values the server may use for a stale `lastID`. */
const CONFLICT_CODES = ['CONFLICT', 'SITE_CONFIG_CONFLICT', 'ErrSiteConfigConflict'];

/**
 * Whether an `updateSiteConfiguration` failure means someone else saved the configuration since it
 * was loaded (the `lastID` is stale). Detected by error code or, failing that, by message.
 */
export function isConflictError(err: unknown): boolean {
	if (!(err instanceof GraphQLRequestError)) return false;
	if (err.status === 409) return true;
	if (CONFLICT_CODES.some((code) => err.hasCode(code))) return true;
	return err.errors.some((e) =>
		/conflict|out of date|stale|modified (by|since)|changed (by|since)|lastID/i.test(e.message)
	);
}
