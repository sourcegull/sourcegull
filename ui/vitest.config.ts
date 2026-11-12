import { svelte } from '@sveltejs/vite-plugin-svelte';
import { defineConfig } from 'vitest/config';

// Unit tests cover TypeScript modules in src/lib. The Svelte plugin compiles the `.svelte.ts`
// modules among them, whose reactive state needs the Svelte compiler.
export default defineConfig({
	plugins: [svelte()],
	test: {
		include: ['src/**/*.test.ts'],
		environment: 'node'
	}
});
