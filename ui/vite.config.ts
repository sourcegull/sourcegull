/// <reference types="node" />
import adapter from '@sveltejs/adapter-static';
import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

/** Backend started with `sourcegull run` (or `sourcegull supervise`). */
const backend = process.env.SOURCEGULL_DEV_BACKEND ?? 'http://127.0.0.1:7080';

export default defineConfig({
	plugins: [
		sveltekit({
			// SPA: every route is client-rendered from the `index.html` fallback, which the
			// binary serves for any path that is not an asset (see crates/web-assets).
			adapter: adapter({ pages: 'build', assets: 'build', fallback: 'index.html', strict: true })
		})
	],
	server: {
		proxy: {
			'/.api': { target: backend, changeOrigin: false },
			'/.auth': { target: backend, changeOrigin: false },
			'/.executors': { target: backend, changeOrigin: false },
			'/-/': { target: backend, changeOrigin: false }
		}
	},
	preview: {
		proxy: {
			'/.api': { target: backend, changeOrigin: false },
			'/.auth': { target: backend, changeOrigin: false },
			'/.executors': { target: backend, changeOrigin: false },
			'/-/': { target: backend, changeOrigin: false }
		}
	}
});
