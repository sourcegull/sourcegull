import { describe, expect, it } from 'vitest';
import {
	combinedOutput,
	computePageURL,
	computeURL,
	exportText,
	isMatchContext,
	originalFileURL,
	resultFileURL,
	streamCompute,
	type ComputeResult,
	type ComputeText
} from './compute.ts';

describe('compute stream', () => {
	it('streams results and reports HTTP errors', async () => {
		const body =
			'event: results\ndata: [{"value":"a\\n","kind":"output"},{"matches":[],"path":"p","repositoryID":1,"repository":"r"}]\n\n' +
			'event: progress\ndata: {"done":true,"matchCount":2,"durationMs":5,"skipped":[]}\n\nevent: done\ndata: {}\n\n';
		const seen: string[] = [];
		const results: ComputeResult[] = [];
		const fakeFetch = (async (url: string) => {
			expect(url).toBe(computeURL('content:output(a -> $1)'));
			return new Response(body, { status: 200 });
		}) as unknown as typeof fetch;
		await streamCompute(
			'content:output(a -> $1)',
			{
				onResults: (r) => results.push(...r),
				onProgress: (p) => seen.push(`p${p.matchCount}`),
				onDone: () => seen.push('done')
			},
			undefined,
			fakeFetch
		);
		expect(seen).toEqual(['p2', 'done']);
		expect(results.map(isMatchContext)).toEqual([false, true]);
		const errFetch = (async () => new Response('bad query', { status: 400 })) as unknown as typeof fetch;
		const errors: string[] = [];
		await streamCompute('x', { onError: (e) => errors.push(e) }, undefined, errFetch);
		expect(errors).toEqual(['bad query']);
		expect(computeURL('a b')).toBe('/.api/compute/stream?q=a+b');
	});
});

describe('compute page helpers', () => {
	const range = (line: number) => ({
		start: { offset: 0, line, column: 0 },
		end: { offset: 0, line, column: 0 }
	});
	const mc: ComputeResult = {
		matches: [
			{ value: 'FooError', range: range(4), environment: {} },
			{ value: 'BarError', range: range(9), environment: {} }
		],
		path: 'src/a b.rs',
		repositoryID: 1,
		repository: 'gitlab.com/acme/app',
		commit: 'abc123'
	};
	const replace: ComputeText = {
		value: 'new\n',
		kind: 'replace-in-place',
		repositoryID: 1,
		repository: 'gitlab.com/acme/app',
		commit: 'abc123',
		path: 'src/main.go'
	};

	it('opens a search query in compute without its pattern type', () => {
		expect(computePageURL('repo:app (\\w+)Error patternType:keyword')).toBe('/compute?q=repo%3Aapp+%28%5Cw%2B%29Error');
		expect(computePageURL('PatternType:regexp foo')).toBe('/compute?q=foo');
	});

	it('exports the output values, else one line per match', () => {
		const outputs: ComputeResult[] = [{ value: 'a\n', kind: 'output' }, mc, { value: 'b\n', kind: 'output' }];
		expect(combinedOutput(outputs)).toBe('a\nb\n');
		expect(exportText(outputs)).toBe('a\nb\n');
		expect(exportText([mc, replace])).toBe(
			'gitlab.com/acme/app:src/a b.rs:5:FooError\ngitlab.com/acme/app:src/a b.rs:10:BarError\n'
		);
		expect(exportText([replace])).toBe('');
	});

	it('links results to the file view and the raw original', () => {
		expect(resultFileURL(mc)).toBe('/gitlab.com/acme/app@abc123/-/blob/src/a%20b.rs');
		expect(resultFileURL({ value: 'x', kind: 'output' })).toBeNull();
		expect(originalFileURL(replace)).toBe('/gitlab.com/acme/app@abc123/-/raw/src/main.go');
		expect(
			originalFileURL({
				value: 'x',
				kind: 'output',
				repository: 'r',
				path: 'p',
				commit: 'c'
			})
		).toBeNull();
	});
});
