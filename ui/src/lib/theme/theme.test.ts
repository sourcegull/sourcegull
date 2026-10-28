import { afterEach, describe, expect, it, vi } from 'vitest';
import {
	SIDEBAR_COLLAPSED_KEY,
	THEME_KEY,
	loadSidebarCollapsed,
	loadTheme,
	parseTheme,
	readStorage,
	storeSidebarCollapsed,
	writeStorage
} from './theme.ts';

function memoryStorage(): Storage {
	const map = new Map<string, string>();
	return {
		get length() {
			return map.size;
		},
		clear: () => map.clear(),
		getItem: (k) => map.get(k) ?? null,
		key: (i) => [...map.keys()][i] ?? null,
		removeItem: (k) => void map.delete(k),
		setItem: (k, v) => void map.set(k, String(v))
	};
}

afterEach(() => {
	vi.unstubAllGlobals();
});

describe('theme preferences', () => {
	it('parses only known themes', () => {
		expect(parseTheme('dark')).toBe('dark');
		expect(parseTheme('light')).toBe('light');
		expect(parseTheme('purple')).toBe('system');
		expect(parseTheme(null)).toBe('system');
	});

	it('round-trips through storage; system removes the key', () => {
		const storage = memoryStorage();
		vi.stubGlobal('localStorage', storage);
		writeStorage(THEME_KEY, 'dark');
		expect(loadTheme()).toBe('dark');
		writeStorage(THEME_KEY, null);
		expect(storage.getItem(THEME_KEY)).toBeNull();
		expect(loadTheme()).toBe('system');
	});

	it('remembers the collapsed sidebar', () => {
		vi.stubGlobal('localStorage', memoryStorage());
		expect(loadSidebarCollapsed()).toBe(false);
		storeSidebarCollapsed(true);
		expect(readStorage(SIDEBAR_COLLAPSED_KEY)).toBe('true');
		expect(loadSidebarCollapsed()).toBe(true);
		storeSidebarCollapsed(false);
		expect(loadSidebarCollapsed()).toBe(false);
	});

	it('survives storage that throws', () => {
		vi.stubGlobal('localStorage', {
			getItem() {
				throw new Error('blocked');
			},
			setItem() {
				throw new Error('blocked');
			},
			removeItem() {
				throw new Error('blocked');
			}
		});
		expect(loadTheme()).toBe('system');
		expect(() => storeSidebarCollapsed(true)).not.toThrow();
		expect(loadSidebarCollapsed()).toBe(false);
	});
});
