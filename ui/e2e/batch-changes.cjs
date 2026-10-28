// Browser check of the bulk changes UI: a spec uploaded like the `gull` CLI does
// (`createChangesetSpecs` + `createBatchSpec`) is previewed at its apply URL (operations,
// publication state toggles, the spec's own `published`, a diff), created, and shown on the
// bulk change page (stats, changesets, a diff, filters); one changeset is marked published and
// merged in the database so the burndown chart has a line and a comment bulk operation applies;
// the bulk operation shows on its tab; the spec tab shows the YAML; the list filters by state;
// adding a GitLab token fails validation without a GitLab; the bulk change is closed and
// deleted. Unpublished changesets only otherwise, so no GitLab is needed. Fails on any page or
// console error. Screenshots go to OUT.
//
// Run it with scripts/test/e2e-ui.sh, after ui/e2e/search.cjs has signed up the admin.
const { execFileSync } = require('node:child_process');
const { chromium } = require('playwright');
const BASE = process.env.BASE || 'http://127.0.0.1:7090';
const OUT = process.env.OUT || require('node:os').tmpdir();
const DB = process.env.SOURCEGULL_DATABASE_URL;
const REPO = 'gitlab.example.com/acme/sourcegull';
const HEADERS = { 'X-Requested-With': 'Sourcegull' };

function step(name) {
	console.log(`- ${name}`);
}

const SPEC = `name: e2e-hello
description: |
  Appends **Hello World** to the README.

  - one changeset per branch
  - see [the docs](https://example.com/docs)
on:
  - repository: ${REPO}
steps:
  - run: echo Hello World >> README.md
    container: alpine:3
changesetTemplate:
  title: Hello World
  body: My first bulk change!
  branch: hello
  commit:
    message: Append Hello World
`;

const DIFF =
	'diff --git a/README.md b/README.md\n--- a/README.md\n+++ b/README.md\n@@ -1 +1,2 @@\n # Sourcegull\n+Hello World\n';

let page;
(async () => {
	const browser = await chromium.launch({ executablePath: process.env.CHROMIUM_EXECUTABLE });
	const context = await browser.newContext({ viewport: { width: 1400, height: 900 }, bypassCSP: true });
	page = await context.newPage();
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
	const psql = (sql) => execFileSync('psql', [DB, '-v', 'ON_ERROR_STOP=1', '-tAqc', sql], { encoding: 'utf8' });
	const row = (title) => `li.changeset[data-changeset="${title}"]`;

	step('sign in');
	await page.goto(`${BASE}/sign-in`);
	await page.fill('#username', 'admin');
	await page.fill('#password', 'correct horse battery');
	await page.click('button[type=submit]');
	await page.waitForURL((url) => !url.pathname.startsWith('/sign-in'));

	step('nav entry and empty list');
	await page.goto(`${BASE}/search`);
	await page.click('nav[aria-label=Main] a:has-text("Bulk changes")');
	await page.waitForURL(/\/bulk-changes$/);
	await page.waitForSelector('li.empty:has-text("No bulk changes yet.")', { timeout: 15000 });

	step('upload changeset specs and a batch spec (CLI style)');
	const { currentUser, repository } = await graphql(
		`query($repo: String!) { currentUser { id } repository(name: $repo) { id } }`,
		{ repo: REPO }
	);
	const spec = (branch, title, published) =>
		JSON.stringify({
			baseRepository: repository.id,
			baseRef: 'refs/heads/main',
			baseRev: '0123456789abcdef0123456789abcdef01234567',
			headRepository: repository.id,
			headRef: `refs/heads/${branch}`,
			title,
			body: `Adds Hello World (${branch}).`,
			commits: [{ message: 'Append Hello World', diff: DIFF, authorName: 'Admin', authorEmail: 'admin@example.com' }],
			...(published === undefined ? {} : { published })
		});
	const { createChangesetSpecs } = await graphql(
		`mutation($specs: [String!]!) { createChangesetSpecs(changesetSpecs: $specs) { id } }`,
		{
			specs: [
				spec('hello-1', 'Hello World 1'),
				spec('hello-2', 'Hello World 2'),
				spec('hello-3', 'Hello World 3', false)
			]
		}
	);
	const { createBatchSpec } = await graphql(
		`mutation($ns: ID!, $spec: String!, $ids: [ID!]!) {
			createBatchSpec(namespace: $ns, batchSpec: $spec, changesetSpecs: $ids) { id applyURL }
		}`,
		{ ns: currentUser.id, spec: SPEC, ids: createChangesetSpecs.map((s) => s.id) }
	);
	if (!createBatchSpec.applyURL.startsWith('/users/admin/bulk-changes/apply/')) {
		fail(`applyURL: ${createBatchSpec.applyURL}`);
	}

	step('preview');
	await page.goto(`${BASE}${createBatchSpec.applyURL}`);
	await page.waitForSelector('h1:has-text("e2e-hello")', { timeout: 15000 });
	await page.waitForSelector('li.entry[data-preview="Hello World 1"]');
	if ((await page.$$('li.entry')).length !== 3) fail('expected 3 preview entries');
	const added = await page.textContent('[data-stat="added"] dd');
	if (added.trim() !== '3') fail(`added: ${added}`);
	await page.waitForSelector('.description strong:has-text("Hello World")');
	// The spec sets `published: false` on hello-3: no toggle there.
	await page.waitForSelector('li.entry[data-preview="Hello World 3"] .publish:has-text("Unpublished (spec)")');
	if (await page.$('li.entry[data-preview="Hello World 3"] select')) fail('a toggle where the spec sets published');
	step('publication toggle changes the operations');
	await page.selectOption('li.entry[data-preview="Hello World 1"] select', 'draft');
	await page.waitForSelector('li.entry[data-preview="Hello World 1"] [data-operation="PUBLISH_DRAFT"]');
	await page.selectOption('li.entry[data-preview="Hello World 1"] select', '');
	await page.waitForSelector('li.entry[data-preview="Hello World 1"] [data-operation="PUBLISH_DRAFT"]', {
		state: 'detached'
	});
	step('preview filters and diff');
	await page.fill('input[aria-label="Search the preview"]', 'World 2');
	await page.click('form[aria-label="Filter the preview"] button[type=submit]');
	await page.waitForFunction(() => document.querySelectorAll('li.entry').length === 1);
	await page.click('li.entry[data-preview="Hello World 2"] button.expand');
	await page.waitForSelector('li.entry .file-diff:has-text("README.md")');
	await page.waitForSelector('li.entry .line.added:has-text("Hello World")');
	await page.screenshot({ path: `${OUT}/batch-changes-01-preview.png`, fullPage: true });
	await page.fill('input[aria-label="Search the preview"]', '');
	await page.click('form[aria-label="Filter the preview"] button[type=submit]');
	await page.waitForFunction(() => document.querySelectorAll('li.entry').length === 3);

	step('create');
	await page.click('.apply-box button:has-text("Create")');
	await page.waitForURL(/\/users\/admin\/bulk-changes\/e2e-hello$/, { timeout: 15000 });
	await page.waitForSelector('h1:has-text("e2e-hello")');
	await page.waitForSelector('[data-stat="total"] dd:has-text("3")');
	await page.waitForSelector(row('Hello World 1'));
	await page.waitForSelector(`${row('Hello World 2')} [data-state="UNPUBLISHED"]`);

	step('mark hello-3 published and merged (burndown data, comment target)');
	psql(`UPDATE changesets SET publication_state = 'PUBLISHED', external_state = 'MERGED', external_id = '42',
		state = 'completed', external_review_state = 'APPROVED', external_check_state = 'PASSED',
		title = 'Hello World 3', external_branch = 'refs/heads/hello-3',
		metadata = jsonb_build_object('created_at', to_char((now() - interval '10 days') AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS"Z"'),
			'web_url', 'https://gitlab.example.com/acme/sourcegull/-/merge_requests/42'),
		last_synced_at = now(), next_sync_at = now() + interval '30 days'
		WHERE current_spec_id IN (SELECT id FROM changeset_specs WHERE head_ref LIKE '%hello-3')`);
	psql(`INSERT INTO changeset_events (changeset_id, kind, key, metadata, created_at)
		SELECT id, 'MERGED', 'e2e-merged', '{}', now() - interval '3 days' FROM changesets WHERE external_id = '42'`);
	await page.reload();
	await page.waitForSelector(`${row('Hello World 3')} [data-state="MERGED"]`, { timeout: 15000 });
	await page.waitForSelector(`${row('Hello World 3')} a[href*="merge_requests/42"]`);
	await page.waitForSelector('[data-stat="merged"] dd:has-text("1")');

	step('changeset filters and diff');
	await page.selectOption('select[aria-label="State"]', 'MERGED');
	await page.waitForFunction(() => document.querySelectorAll('li.changeset').length === 1);
	await page.selectOption('select[aria-label="State"]', '');
	// A quoted phrase: the terms `World` and `2` would also match hello-3 (external ID 42).
	await page.fill('input[aria-label="Search changesets"]', '"World 2"');
	await page.click('form[aria-label="Filter changesets"] button[type=submit]');
	await page.waitForFunction(() => document.querySelectorAll('li.changeset').length === 1);
	await page.click(`${row('Hello World 2')} button.expand`);
	await page.waitForSelector(`${row('Hello World 2')} .file-diff:has-text("README.md")`);
	await page.waitForSelector(`${row('Hello World 2')} .body:has-text("Adds Hello World (hello-2).")`);
	await page.fill('input[aria-label="Search changesets"]', '');
	await page.click('form[aria-label="Filter changesets"] button[type=submit]');
	await page.waitForFunction(() => document.querySelectorAll('li.changeset').length === 3);
	await page.screenshot({ path: `${OUT}/batch-changes-02-detail.png`, fullPage: true });

	step('bulk comment');
	await page.check(`${row('Hello World 3')} input[type=checkbox]`);
	await page.waitForSelector('#bulk-action option[value="COMMENT"]:not([disabled])', { state: 'attached', timeout: 10000 });
	if (!(await page.$('#bulk-action option[value="MERGE"][disabled]'))) fail('merge offered for a merged changeset');
	await page.selectOption('#bulk-action', 'COMMENT');
	await page.fill('#bulk-comment', 'Hello from the e2e check');
	await page.click('form[aria-label="Bulk action"] button[type=submit]');
	await page.waitForSelector('.alert-success:has-text("Comment started for 1 changeset")');
	step('CSV export');
	const [download] = await Promise.all([page.waitForEvent('download'), page.click('button:has-text("Export CSV")')]);
	if (download.suggestedFilename() !== 'e2e-hello-changesets.csv') fail(`csv name: ${download.suggestedFilename()}`);
	const csv = require('node:fs').readFileSync(await download.path(), 'utf8');
	if (!csv.includes('Hello World 3') || csv.split('\n').filter((l) => l.trim()).length < 4) fail(`csv: ${csv}`);

	step('bulk operations tab');
	await page.click('nav[aria-label="Bulk change"] a:has-text("Bulk operations")');
	await page.waitForSelector('li.operation[data-bulk-operation="COMMENT"]:has-text("Commented on 1 changeset")');
	// The worker runs it against a GitLab that does not exist: it ends failed, with the error.
	await page.waitForSelector('li.operation [data-state="FAILED"], li.operation [data-state="COMPLETED"]', {
		timeout: 30000
	});
	await page.screenshot({ path: `${OUT}/batch-changes-03-bulk.png`, fullPage: true });

	step('burndown chart');
	await page.click('nav[aria-label="Bulk change"] a:has-text("Burndown chart")');
	await page.waitForSelector('svg path[data-series="merged"]', { timeout: 15000 });
	const d = await page.getAttribute('svg path[data-series="merged"]', 'd');
	if ((d.match(/L/g) || []).length < 5) fail(`merged line too short: ${d}`);
	await page.hover('.plot svg', { position: { x: 300, y: 100 } });
	await page.waitForSelector('.tooltip:has-text("Merged")');
	await page.screenshot({ path: `${OUT}/batch-changes-04-burndown.png` });
	await page.click('button:has-text("Show table")');
	const last = await page.textContent('table[aria-label="Changesets over time"] tbody tr:first-child');
	if (!/1/.test(last)) fail(`burndown table: ${last}`);

	step('spec tab');
	await page.click('nav[aria-label="Bulk change"] a:has-text("Spec")');
	await page.waitForSelector('pre.spec:has-text("changesetTemplate:")');
	await page.waitForSelector('a:has-text("Edit spec")');

	step('list with state filter');
	await page.goto(`${BASE}/bulk-changes`);
	await page.waitForSelector('li[data-batch-change="e2e-hello"]');
	await page.click('button[role=tab]:has-text("Closed")');
	await page.waitForSelector('li.empty:has-text("No closed bulk changes.")');
	await page.click('button[role=tab]:has-text("Open")');
	await page.waitForSelector('li[data-batch-change="e2e-hello"]');
	await page.screenshot({ path: `${OUT}/batch-changes-05-list.png` });
	await page.goto(`${BASE}/users/admin/bulk-changes`);
	await page.waitForSelector('li[data-batch-change="e2e-hello"]');

	step('credentials: a token that GitLab cannot confirm');
	const { addExternalService } = await graphql(
		`mutation($input: AddExternalServiceInput!) { addExternalService(input: $input) { id } }`,
		{
			input: {
				kind: 'GITLAB',
				displayName: 'E2E GitLab',
				config: JSON.stringify({ url: 'https://gitlab.invalid', token: 'glpat-e2e', projectQuery: ['none'] })
			}
		}
	);
	await page.goto(`${BASE}/site-admin/bulk-changes`);
	await page.waitForSelector('li.host:has-text("gitlab.invalid")', { timeout: 15000 });
	await page.click('li.host:has-text("gitlab.invalid") button:has-text("Add token")');
	await page.fill('#credential-token', 'glpat-not-valid');
	await page.click('form[aria-label="Add token"] button[type=submit]');
	await page.waitForSelector('form[aria-label="Add token"] .alert-danger', { timeout: 30000 });
	await page.screenshot({ path: `${OUT}/batch-changes-06-credentials.png` });
	await page.goto(`${BASE}/user/settings/bulk-changes`);
	await page.waitForSelector('li.host:has-text("gitlab.invalid") .badge:has-text("No token")');
	await graphql(`mutation($id: ID!) { deleteExternalService(externalService: $id) { alwaysNil } }`, {
		id: addExternalService.id
	});

	step('phone layout');
	await page.setViewportSize({ width: 390, height: 844 });
	await page.goto(`${BASE}/users/admin/bulk-changes/e2e-hello`);
	await page.waitForSelector(row('Hello World 1'));
	const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
	if (overflow > 1) fail(`horizontal page scroll on a phone: ${overflow}px`);
	await page.screenshot({ path: `${OUT}/batch-changes-07-phone.png`, fullPage: true });
	await page.setViewportSize({ width: 1400, height: 900 });

	step('close, then delete');
	await page.click('.page-header button:has-text("Close")');
	await page.click('form[aria-label="Close bulk change"] button[type=submit]');
	await page.waitForSelector('.heading [data-state="CLOSED"]');
	await page.click('.page-header button:has-text("Delete")');
	await page.waitForURL(/\/bulk-changes$/);
	await page.waitForSelector('li.empty:has-text("No bulk changes yet.")');

	if (problems.length) fail(problems.join('\n'));
	await browser.close();
	console.log('ok');
})().catch(async (e) => {
	console.error(e);
	if (page) await page.screenshot({ path: `${OUT}/batch-changes-failed.png`, fullPage: true }).catch(() => {});
	process.exit(1);
});
