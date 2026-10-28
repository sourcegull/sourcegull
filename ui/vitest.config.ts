import { defineConfig } from 'vitest/config';

// Unit tests cover plain TypeScript modules in src/lib, so they don't need the SvelteKit plugin.
export default defineConfig({
	test: {
		include: ['src/**/*.test.ts'],
		environment: 'node'
	}
});
