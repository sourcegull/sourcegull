// Browser check of auto-indexing: with codeIntelAutoIndexing.enabled,
// the site admin's inference script page saves a script and tries it on the repository; the
// repository's code graph page shows the inferred configuration (Rust at the root,
// TypeScript in ui/), saves it as the repository's configuration and queues the jobs; the
// jobs show up as precise indexes queued for indexing; an executor (driven here through the
// executor queue API) runs one and fails, and its steps and log output show on the page;
// reindexing queues it again. Fails on any page or console error. Screenshots go to OUT.
//
// Run it with scripts/test/e2e-ui.sh, after ui/e2e/search.cjs has signed up the admin.
const { chromium } = require('playwright');
const BASE = process.env.BASE || 'http://127.0.0.1:7090';
const OUT = process.env.OUT || require('node:os').tmpdir();
const REPO = 'gitlab.example.com/acme/sourcegull';
const HEADERS = { 'X-Requested-With': 'Sourcegull' };
const EXECUTOR_TOKEN = 'e2e-executor-token-0123456789';

function step(name) {
	console.log(`- ${name}`);
}

(async () => {
	const browser = await chromium.launch({ executablePath: process.env.CHROMIUM_EXECUTABLE });
	const context = await browser.newContext({ viewport: { width: 1400, height: 900 }, bypassCSP: true });
	const page = await context.newPage();
	const problems = [];
	page.on('pageerror', (e) => problems.push(`pageerror: ${e.message}`));
	page.on('console', (m) => {
		if (m.type() === 'error') problems.push(`console: ${m.text()}`);
	});
	page.on('dialog', (d) => d.accept());
	const fail = (msg) => {
		throw new Error(msg);
	};
	const graphql = async (query, variables = {}) => {
		const r = await page.request.post(`${BASE}/.api/graphql`, { headers: HEADERS, data: { query, variables } });
		const body = await r.json();
		if (body.errors) fail(`graphql: ${JSON.stringify(body.errors)}`);
		return body.data;
	};
	const executor = async (action, data) => {
		const r = await page.request.post(`${BASE}/.executors/queue/codeintel/${action}`, {
			headers: { ...HEADERS, Authorization: `token-executor ${EXECUTOR_TOKEN}` },
			data: { executorName: 'e2e-executor', ...data }
		});
		if (r.status() >= 300) fail(`${action}: ${r.status()} ${await r.text()}`);
		return r.status() === 200 ? r.json() : null;
	};

	step('sign in');
	await page.goto(`${BASE}/sign-in`);
	await page.fill('#username', 'admin');
	await page.fill('#password', 'correct horse battery');
	await page.click('button[type=submit]');
	await page.waitForURL((url) => !url.pathname.startsWith('/sign-in'));

	step('enable auto-indexing and executors');
	const { site } = await graphql('{ site { configuration { id effectiveContents } } }');
	const contents = site.configuration.effectiveContents.replace(
		'{',
		`{\n  "codeIntelAutoIndexing.enabled": true,\n  "executors.accessToken": "${EXECUTOR_TOKEN}",`
	);
	await graphql('mutation($id: Int!, $input: String!) { updateSiteConfiguration(lastID: $id, input: $input) }', {
		id: site.configuration.id,
		input: contents
	});

	step('the inference script page tries and saves a script');
	await page.goto(`${BASE}/site-admin/inference-script`);
	await page.waitForSelector('#inference-script', { timeout: 15000 });
	await page.fill(
		'#inference-script',
		'print("hello from the e2e script")\nreturn require("sg.autoindex.config").new({ ["sg.go"] = false })'
	);
	await page.fill('#try-repo', REPO);
	for (let i = 0; ; i++) {
		await page.click('button:has-text("Infer jobs")');
		await page.waitForSelector('table[aria-label="Inferred jobs"], .alert-danger', { timeout: 30000 });
		if (await page.$('table[aria-label="Inferred jobs"]')) break;
		// The site config change reaches the web role within a moment.
		if (i > 20) fail(`infer: ${await page.textContent('.alert-danger')}`);
		await page.waitForTimeout(500);
	}
	const inferredTable = await page.textContent('table[aria-label="Inferred jobs"]');
	if (!inferredTable.includes('scip-rust') || !inferredTable.includes('scip-typescript')) fail(`inferred: ${inferredTable}`);
	if (!(await page.textContent('pre.output')).includes('hello from the e2e script')) fail('no script output');
	await page.click('button:has-text("Save")');
	await page.waitForSelector('.alert-info:has-text("Saved the inference script")', { timeout: 10000 });
	await page.screenshot({ path: `${OUT}/autoindex-01-inference-script.png` });

	step('the index configuration tab shows the inferred jobs and saves them');
	await page.goto(`${BASE}/${REPO}/-/code-graph`);
	await page.click('button[role=tab]:has-text("Index configuration")');
	await page.waitForSelector('ul[aria-label="Inferred jobs"]', { timeout: 30000 });
	const inferred = await page.textContent('ul[aria-label="Inferred jobs"]');
	if (!inferred.includes('scip-rust') || !inferred.includes('ui')) fail(`inferred configuration: ${inferred}`);
	await page.click('button:has-text("Use inferred configuration")');
	if (!(await page.inputValue('#index-config')).includes('index_jobs')) fail('the inferred configuration was not copied');
	await page.click('button:has-text("Save")');
	await page.waitForSelector('.alert-info:has-text("Saved")', { timeout: 10000 });
	await page.screenshot({ path: `${OUT}/autoindex-02-configuration.png` });

	step('queue the jobs');
	await page.click('button:has-text("Queue jobs at HEAD")');
	await page.waitForSelector('.alert-info:has-text("Queued 2 index jobs")', { timeout: 30000 });

	step('the jobs are precise indexes queued for indexing');
	await page.click('button[role=tab]:has-text("Precise indexes")');
	const uiRow = 'tr[data-root="ui/"]';
	await page.waitForSelector(`${uiRow}:has-text("Queued for indexing")`, { timeout: 15000 });
	await page.waitForSelector('tr[data-root=""]:has-text("Queued for indexing")', { timeout: 15000 });

	step('an executor runs a job and it fails');
	const job = await executor('dequeue', { version: '0.1.0', capabilities: ['docker-steps', 'scip-upload'] });
	if (!job) fail('no job to dequeue');
	await executor('addExecutionLogEntry', {
		jobId: job.id,
		entry: { key: 'setup.git.fetch', command: ['git', 'fetch'], exitCode: 0, out: 'fetched the commit', durationMs: 12 }
	});
	await executor('addExecutionLogEntry', {
		jobId: job.id,
		entry: { key: 'step.docker.indexer', command: ['sh', '-c'], exitCode: 1, out: 'indexer exploded', durationMs: 34 }
	});
	await executor('markFailed', { jobId: job.id, errorMessage: 'step.docker.indexer exited with 1' });
	await page.reload();
	await page.click('button[role=tab]:has-text("Precise indexes")').catch(() => {});
	const failed = `tr:has(.badge:has-text("Indexing errored"))`;
	await page.waitForSelector(failed, { timeout: 15000 });
	await page.click(`${failed} + tr.detail button:has-text("Index job logs")`);
	await page.waitForSelector('ol[aria-label="Index job steps"] pre:has-text("indexer exploded")', { timeout: 10000 });
	const logs = await page.textContent('ol[aria-label="Index job steps"]');
	if (!logs.includes('exit 1') || !logs.includes('fetched the commit')) fail(`logs: ${logs}`);
	await page.screenshot({ path: `${OUT}/autoindex-03-logs.png`, fullPage: true });

	step('reindex queues it again');
	await page.click(`${failed} button:has-text("Reindex")`);
	await page.waitForSelector('.alert-info:has-text("for indexing again")', { timeout: 10000 });
	await page.waitForSelector(`${failed} .badge:has-text("reindexing")`, { timeout: 10000 });

	step('phone layout');
	await page.setViewportSize({ width: 390, height: 844 });
	await page.goto(`${BASE}/site-admin/inference-script`);
	await page.waitForSelector('#inference-script');
	const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
	if (overflow > 1) fail(`horizontal page scroll on a phone: ${overflow}px`);

	// Leave the script and configuration as they were for later checks.
	await graphql('mutation { updateCodeIntelligenceInferenceScript(script: "") { alwaysNil } }');
	if (problems.length) fail(problems.join('\n'));
	await browser.close();
	console.log('ok');
})().catch((e) => {
	console.error(e);
	process.exit(1);
});
