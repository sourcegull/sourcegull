/**
 * Forward cursor paging with a way back: the cursors of the pages before the current one, and
 * the next page's cursor from the last response (`pageInfo.endCursor`). A plain object, so a
 * page keeps it in `$state` and the functions below change it in place.
 */
export interface CursorPages {
	/** Rows per page. */
	readonly size: number;
	/** The `after` cursors of the pages up to the current one (`null` for the first). */
	cursors: (string | null)[];
	/** The next page's cursor, or null on the last page. */
	next: string | null;
}

export function cursorPages(size: number): CursorPages {
	return { size, cursors: [null], next: null };
}

/** The `after` argument for the current page. */
export const pageAfter = (p: CursorPages): string | null => p.cursors[p.cursors.length - 1] ?? null;

/** The current page, from 0. */
export const pageIndex = (p: CursorPages): number => p.cursors.length - 1;

export const hasPreviousPage = (p: CursorPages): boolean => p.cursors.length > 1;

export const hasNextPage = (p: CursorPages): boolean => p.next !== null;

/** Records the `pageInfo` of the page just loaded. */
export function pageLoaded(p: CursorPages, info: { hasNextPage: boolean; endCursor: string | null }): void {
	p.next = info.hasNextPage ? info.endCursor : null;
}

/** Back to the first page, after the filter or the order changed. */
export function firstPage(p: CursorPages): void {
	p.cursors = [null];
	p.next = null;
}

export function nextPage(p: CursorPages): void {
	if (p.next !== null) p.cursors = [...p.cursors, p.next];
}

export function previousPage(p: CursorPages): void {
	if (p.cursors.length > 1) p.cursors = p.cursors.slice(0, -1);
}
