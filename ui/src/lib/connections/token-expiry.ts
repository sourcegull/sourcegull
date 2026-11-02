/**
 * How a connection's token expiry is shown: the stored check (`ExternalService.tokenExpiry`)
 * and the expiry a connection check reports.
 */

/** Days before expiry from which the token is shown as expiring soon. */
export const SOON_DAYS = 14;

export interface TokenExpiry {
	expiresAt: string | null;
	neverExpires: boolean;
	error?: string | null;
}

export type ExpiryLevel = 'ok' | 'soon' | 'expired' | 'unknown';

/** Whole days until `iso`, rounded up; 0 once it has passed. */
export function daysLeft(iso: string, now: number = Date.now()): number {
	const ms = Date.parse(iso) - now;
	return ms <= 0 ? 0 : Math.ceil(ms / 86_400_000);
}

/** The date part of `iso` (UTC), e.g. `2026-11-22`. */
export function expiryDate(iso: string): string {
	return new Date(iso).toISOString().slice(0, 10);
}

export function expiryLevel(e: TokenExpiry | null, now: number = Date.now()): ExpiryLevel {
	if (!e) return 'unknown';
	if (e.neverExpires) return 'ok';
	if (!e.expiresAt) return 'unknown';
	if (Date.parse(e.expiresAt) <= now) return 'expired';
	return daysLeft(e.expiresAt, now) <= SOON_DAYS ? 'soon' : 'ok';
}

/** E.g. "expires 2026-11-22 (in 10 days)", "no expiry date", "expired 2026-11-22". */
export function expiryText(e: TokenExpiry, now: number = Date.now()): string {
	if (e.neverExpires) return 'no expiry date';
	if (!e.expiresAt) return 'unknown';
	const date = expiryDate(e.expiresAt);
	if (Date.parse(e.expiresAt) <= now) return `expired ${date}`;
	const days = daysLeft(e.expiresAt, now);
	return `expires ${date} (${days === 1 ? 'tomorrow' : `in ${days} days`})`;
}
