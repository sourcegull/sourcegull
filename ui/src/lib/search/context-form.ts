/** The search context form's value and its repository list format. */

/** Parses `name[@rev1,rev2]` lines; revisions default to `HEAD`. */
export function parseRepositoryLines(text: string): { name: string; revisions: string[] }[] {
	return text
		.split('\n')
		.map((l) => l.trim())
		.filter((l) => l !== '' && !l.startsWith('#'))
		.map((l) => {
			const at = l.indexOf('@');
			if (at < 0) return { name: l, revisions: ['HEAD'] };
			const revs = l
				.slice(at + 1)
				.split(',')
				.map((r) => r.trim())
				.filter(Boolean);
			return { name: l.slice(0, at), revisions: revs.length ? revs : ['HEAD'] };
		});
}

export function formatRepositoryLines(repos: { name: string; revisions: string[] }[]): string {
	return repos
		.map((r) =>
			r.revisions.length === 1 && r.revisions[0] === 'HEAD' ? r.name : `${r.name}@${r.revisions.join(',')}`
		)
		.join('\n');
}

export interface ContextFormValue {
	namespace: string | null;
	name: string;
	description: string;
	public: boolean;
	kind: 'query' | 'repositories';
	query: string;
	repositories: string;
}
