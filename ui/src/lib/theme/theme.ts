/**
 * Colour theme and sidebar preferences, kept in localStorage (per browser).
 *
 * The theme is applied as `<html data-theme="light|dark">`; without the attribute the CSS follows
 * `prefers-color-scheme` (app.css). app.html applies the stored theme before first paint, with
 * the same key, so that a dark-mode user does not see a light flash.
 */

export type ThemePreference = 'system' | 'light' | 'dark';

export const THEME_KEY = 'sourcegull.theme';
export const SIDEBAR_COLLAPSED_KEY = 'sourcegull.sidebar.collapsed';

/** localStorage access that never throws (private windows, blocked storage, SSR). */
export function readStorage(key: string): string | null {
	try {
		return globalThis.localStorage?.getItem(key) ?? null;
	} catch {
		return null;
	}
}

export function writeStorage(key: string, value: string | null): void {
	try {
		if (value === null) globalThis.localStorage?.removeItem(key);
		else globalThis.localStorage?.setItem(key, value);
	} catch {
		// Storage unavailable: the preference lasts for this page only.
	}
}

export function parseTheme(value: string | null): ThemePreference {
	return value === 'light' || value === 'dark' ? value : 'system';
}

export function loadTheme(): ThemePreference {
	return parseTheme(readStorage(THEME_KEY));
}

/** Stores the preference and applies it to `root` (the document element by default). */
export function setTheme(theme: ThemePreference, root: HTMLElement | undefined = globalThis.document?.documentElement): void {
	writeStorage(THEME_KEY, theme === 'system' ? null : theme);
	if (!root) return;
	if (theme === 'system') root.removeAttribute('data-theme');
	else root.setAttribute('data-theme', theme);
}

export function loadSidebarCollapsed(): boolean {
	return readStorage(SIDEBAR_COLLAPSED_KEY) === 'true';
}

export function storeSidebarCollapsed(collapsed: boolean): void {
	writeStorage(SIDEBAR_COLLAPSED_KEY, collapsed ? 'true' : null);
}
