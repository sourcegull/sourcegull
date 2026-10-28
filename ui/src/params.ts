import { defineParams } from '@sveltejs/kit/params';
import { isNamespaceKind } from '#lib/batches/batch-change.ts';

/** Route parameter matchers (`[name=matcher]`). */
export const params = defineParams({
	/** `users` or `organizations`: the namespace part of bulk change URLs (`BatchChange.url`). */
	namespace: (param: string) => (isNamespaceKind(param) ? param : undefined)
});
