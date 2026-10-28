// Browser check of creating and running batch specs: the create page
// makes a draft bulk change; the spec editor shows a validation error with its line and
// column, then, fixed, the resolved workspace of the repository; running the spec opens the
// execution page, where cancel and retry work; an executor (driven here through the executor
// queue API) fails the workspace, whose step output and failure show; retrying the workspace
// and completing it with a changeset spec completes the execution, with the preview link.
// With batchChanges.enabled off the pages say so. Fails on any page or console error.
// Screenshots go to OUT.
//
// Run it with scripts/test/e2e-ui.sh, after ui/e2e/search.cjs has signed up the admin and with
// the repository it sets up.
const { chromium } = require('playwright');
const BASE = process.env.BASE || 'http://127.0.0.1:7090';
const OUT = process.env.OUT || require('node:os').tmpdir();
const REPO = 'gitlab.example.com/acme/sourcegull';
const HEADERS = { 'X-Requested-With': 'Sourcegull' };
const EXECUTOR_TOKEN = 'e2e-executor-token-0123456789';
const NAME = 'e2e-hello';
const EDIT = `/users/admin/bulk-changes/${NAME}/edit`;

const SPEC = (container) => `version: 2
name: ${NAME}
description: Say hello in the README
on:
  - repository: ${REPO}
steps:
  - run: echo Hello >> README.md
${container}changesetTemplate:
  title: Say hello
  body: Adds a greeting.
  branch: e2e-hello
  commit:
    message: Say hello
`;
const INVALID = SPEC('');
const VALID = SPEC('    container: alpine:3\n');

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
	const post = async (path, data, authorization) => {
		const r = await page.request.post(`${BASE}${path}`, { headers: { ...HEADERS, Authorization: authorization }, data });
		if (r.status() >= 300) fail(`${path}: ${r.status()} ${await r.text()}`);
		return r.status() === 200 ? r.json() : null;
	};
	const executor = (action, data) =>
		post(`/.executors/queue/batches/${action}`, { executorName: 'e2e-executor', ...data }, `token-executor ${EXECUTOR_TOKEN}`);
	const dequeue = async () => {
		for (let i = 0; i < 40; i++) {
			const job = await executor('dequeue', { version: '0.1.0', capabilities: ['batch-steps', 'executor-secrets'] });
			if (job) return job;
			await page.waitForTimeout(250);
		}
		fail('no batches job to dequeue');
	};
	const log = (jobId, entry) => executor('addExecutionLogEntry', { jobId, entry });
	const setConfig = async (edit) => {
		const { site } = await graphql('{ site { configuration { id effectiveContents } } }');
		const next = edit(site.configuration.effectiveContents);
		if (next === site.configuration.effectiveContents) return;
		await graphql('mutation($id: Int!, $input: String!) { updateSiteConfiguration(lastID: $id, input: $input) }', {
			id: site.configuration.id,
			input: next
		});
	};
	/** Replaces the CodeMirror document. */
	const setSpec = async (text) => {
		await page.click('.cm-content');
		await page.keyboard.press('Control+A');
		await page.keyboard.press('Delete');
		await page.keyboard.insertText(text);
	};
	const specState = (label) => page.waitForSelector(`[data-testid=spec-state]:has-text("${label}")`, { timeout: 20000 });

	step('sign in');
	await page.goto(`${BASE}/sign-in`);
	await page.fill('#username', 'admin');
	await page.fill('#password', 'correct horse battery');
	await page.click('button[type=submit]');
	await page.waitForURL((url) => !url.pathname.startsWith('/sign-in'));

	step('executors can connect');
	await setConfig((c) =>
		c.includes('"executors.accessToken"') ? c : c.replace('{', `{\n  "executors.accessToken": "${EXECUTOR_TOKEN}",`)
	);

	step('create a bulk change');
	await page.goto(`${BASE}/bulk-changes/create`);
	await page.waitForSelector('#bc-name', { timeout: 15000 });
	await page.fill('#bc-name', 'not valid!');
	if (!(await page.isDisabled('form button[type=submit]'))) fail('an invalid name can be submitted');
	await page.fill('#bc-name', NAME);
	await page.screenshot({ path: `${OUT}/batch-specs-01-create.png` });
	await page.click('form button[type=submit]');
	await page.waitForURL((url) => url.pathname === EDIT, { timeout: 15000 });

	step('the editor starts from the hello world example');
	await page.waitForSelector('.cm-content', { timeout: 15000 });
	const initial = await page.textContent('.cm-content');
	if (!initial.includes(`name: ${NAME}`) || !initial.includes('changesetTemplate')) fail(`initial spec: ${initial}`);

	step('a validation error with its position');
	await setSpec(INVALID);
	await page.waitForSelector('ul[aria-label="Spec errors"]', { timeout: 15000 });
	const errors = await page.textContent('ul[aria-label="Spec errors"]');
	if (!/line \d+, column \d+/.test(errors) || !errors.includes('container')) fail(`errors: ${errors}`);
	await page.waitForSelector('.cm-lintRange-error');
	if (!(await page.isDisabled('button:has-text("Run bulk change spec")'))) fail('an invalid spec can be run');
	await page.screenshot({ path: `${OUT}/batch-specs-02-invalid.png` });

	step('fixed, the workspaces resolve');
	await setSpec(VALID);
	await page.waitForSelector('.validation[data-state=valid]', { timeout: 15000 });
	await page.waitForSelector(`ul[aria-label=Workspaces] li[data-repo="${REPO}"]`, { timeout: 30000 });
	const ws = await page.textContent(`li[data-repo="${REPO}"]`);
	if (!ws.includes('main')) fail(`workspace: ${ws}`);
	if ((await page.$$('ul[aria-label="Spec errors"]')).length) fail('errors still shown');
	await page.waitForSelector('button:has-text("Run bulk change spec"):not([disabled])', { timeout: 10000 });
	await page.screenshot({ path: `${OUT}/batch-specs-03-workspaces.png` });

	step('examples replace the spec, keeping the name');
	await page.selectOption('#spec-example', 'sed-replace');
	await page.click('button:has-text("Insert")');
	await page.waitForFunction(() => document.querySelector('.cm-content')?.textContent?.includes('allowlist'));
	if (!(await page.textContent('.cm-content')).includes(`name: ${NAME}`)) fail('the example kept its own name');
	await setSpec(VALID);
	await page.waitForSelector('.validation[data-state=valid]', { timeout: 15000 });
	await page.waitForSelector(`li[data-repo="${REPO}"]`, { timeout: 30000 });

	step('run it on the server');
	await page.click('button:has-text("Run bulk change spec"):not([disabled])', { timeout: 10000 });
	await page.waitForURL(/\/executions\//, { timeout: 15000 });
	await specState('Queued');
	const execURL = page.url();
	const specID = decodeURIComponent(execURL.split('/executions/')[1]);
	await page.click(`button.ws[data-repo="${REPO}"]`);
	await page.waitForSelector('[data-testid=workspace-state]:has-text("Queued")');
	await page.screenshot({ path: `${OUT}/batch-specs-04-queued.png` });

	step('cancel and retry the execution');
	await page.click('button:has-text("Cancel")');
	await specState('Canceled');
	await page.click('button:has-text("Retry failed workspaces")');
	await specState('Queued');

	step('an executor fails the workspace');
	let job = await dequeue();
	if (job.batches.repository.name !== REPO) fail(`job: ${JSON.stringify(job.batches)}`);
	const now = new Date().toISOString();
	await log(job.id, { key: 'setup.git.fetch', command: ['git', 'fetch'], exitCode: 0, out: 'fetched', durationMs: 20, startTime: now });
	await specState('Processing');
	await log(job.id, {
		key: 'step.0',
		command: ['alpine:3', 'echo Hello >> README.md'],
		exitCode: 1,
		out: 'stdout: about to fail\nstderr: README.md: read-only file system\n',
		durationMs: 120,
		startTime: now
	});
	await executor('markFailed', { jobId: job.id, errorMessage: 'step 1 exited with 1' });
	await specState('Failed');
	await page.waitForSelector('[data-testid=workspace-state]:has-text("Failed")', { timeout: 15000 });
	await page.waitForSelector('[data-testid=workspace-failure]:has-text("exited with 1")');
	const output = await page.textContent('pre[aria-label="Step 1 output"]');
	if (!output.includes('read-only file system') || !output.includes('about to fail')) fail(`step output: ${output}`);
	if (!(await page.textContent('ol[aria-label=Steps]')).includes('exit 1')) fail('no exit code');
	await page.screenshot({ path: `${OUT}/batch-specs-05-failed.png`, fullPage: true });

	step('retry the workspace; it completes with a changeset spec');
	await page.click('button:has-text("Retry workspace")');
	await specState('Queued');
	job = await dequeue();
	const b = job.batches;
	await log(job.id, { key: 'setup.git.fetch', command: ['git', 'fetch'], exitCode: 0, out: 'fetched', durationMs: 20, startTime: now });
	await log(job.id, {
		key: 'step.0',
		command: ['alpine:3', 'echo Hello >> README.md'],
		exitCode: 0,
		out: 'stdout: wrote the greeting\n',
		durationMs: 90,
		startTime: now
	});
	const changesetSpec = {
		baseRepository: b.repository.id,
		baseRef: b.repository.baseRef,
		baseRev: b.repository.baseRev,
		headRepository: b.repository.id,
		headRef: 'refs/heads/e2e-hello',
		title: 'Say hello',
		body: 'Adds a greeting.',
		commits: [
			{
				message: 'Say hello',
				diff: 'diff --git README.md README.md\n--- README.md\n+++ README.md\n@@ -1 +1,2 @@\n # sg_search\n+Hello\n',
				authorName: 'admin',
				authorEmail: 'admin@example.com'
			}
		],
		published: false
	};
	await post('/.executors/queue/batches/uploadResult', { changesetSpecs: [JSON.stringify(changesetSpec)] }, `Bearer ${job.token}`);
	await executor('markComplete', { jobId: job.id });
	await specState('Completed');
	await page.waitForSelector('[data-testid=workspace-state]:has-text("Completed")', { timeout: 15000 });
	await page.waitForSelector('ul.changesets:has-text("Say hello")', { timeout: 15000 });
	const counts = await page.textContent('[data-testid=execution-counts]');
	if (!counts.includes('1 of 1 workspaces finished')) fail(`counts: ${counts}`);
	const { node } = await graphql('query($id: ID!) { node(id: $id) { ... on BatchSpec { applyURL } } }', { id: specID });
	if (!node.applyURL) fail('no applyURL');
	const href = await page.getAttribute('[data-testid=preview-link]', 'href');
	if (href !== node.applyURL) fail(`preview link ${href}, applyURL ${node.applyURL}`);
	await page.screenshot({ path: `${OUT}/batch-specs-06-completed.png`, fullPage: true });

	step('the editor now creates a new spec');
	await page.goto(`${BASE}${EDIT}`);
	await page.waitForSelector('.alert-info:has-text("This spec has run")', { timeout: 15000 });

	step('phone layout');
	await page.setViewportSize({ width: 390, height: 844 });
	for (const url of [execURL, `${BASE}${EDIT}`, `${BASE}/bulk-changes/create`]) {
		await page.goto(url);
		await page.waitForSelector('h1', { timeout: 15000 });
		await page.waitForTimeout(500);
		const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
		if (overflow > 1) fail(`horizontal page scroll on a phone at ${url}: ${overflow}px`);
	}
	await page.screenshot({ path: `${OUT}/batch-specs-07-phone.png` });
	await page.setViewportSize({ width: 1400, height: 900 });

	step('with bulk changes disabled the pages say so');
	await setConfig((c) => c.replace('{', '{\n  "batchChanges.enabled": false,'));
	for (let i = 0; ; i++) {
		await page.goto(`${BASE}/bulk-changes/create`);
		await page.waitForSelector('.alert', { timeout: 15000 });
		if ((await page.textContent('.alert')).includes('disabled')) break;
		if (i > 20) fail(`not disabled: ${await page.textContent('.alert')}`);
		await page.waitForTimeout(500);
	}
	if (await page.$('#bc-name')) fail('the form is shown while disabled');
	await page.goto(`${BASE}${EDIT}`);
	await page.waitForSelector('.alert-warning:has-text("disabled")', { timeout: 15000 });
	if (await page.$('.cm-content')) fail('the editor is shown while disabled');
	await setConfig((c) => c.replace(/\n\s*"batchChanges\.enabled": false,/, ''));

	if (problems.length) fail(problems.join('\n'));
	await browser.close();
	console.log('ok');
})().catch((e) => {
	console.error(e);
	process.exit(1);
});
