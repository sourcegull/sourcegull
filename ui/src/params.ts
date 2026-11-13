import { defineParams } from '@sveltejs/kit/params';

/**
 * Route parameter matchers (`[name=matcher]`). The build loads this file with Node, without the
 * Svelte compiler, so it imports nothing from `#lib`: a module there that uses runes, directly or
 * through its imports, fails the build.
 */
export const params = defineParams({
	/** `users` or `organizations`: the namespace part of bulk change URLs (`BatchChange.url`). */
	namespace: (param: string) => (param === 'users' || param === 'organizations' ? param : undefined)
});
