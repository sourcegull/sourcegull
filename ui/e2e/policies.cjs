// Browser check of code intelligence configuration policies and retention: the site admin page lists the built-in policies, creates a policy with a
// repository pattern (and its preview), edits and deletes it; a built-in policy can't be
// renamed or deleted; the repository's code graph page lists its policies and creates one for
// the repository with a branch preview; an uploaded index shows that the tip of the default
// branch keeps it. Fails on any page or console error. Screenshots go to OUT.
//
// Run it with scripts/test/e2e-ui.sh, after ui/e2e/search.cjs has signed up the admin.
const { chromium } = require('playwright');
const BASE = process.env.BASE || 'http://127.0.0.1:7090';
const OUT = process.env.OUT || require('node:os').tmpdir();
const REPO = 'gitlab.example.com/acme/sourcegull';
const HEADERS = { 'X-Requested-With': 'Sourcegull' };

function step(name) {
	console.log(`- ${name}`);
}

// Protobuf encoding, just enough for a SCIP index with one document.
function varint(n) {
	const out = [];
	while (n > 0x7f) {
		out.push((n & 0x7f) | 0x80);
		n >>>= 7;
	}
	out.push(n);
	return out;
}
const bytes = (field, payload) => [...varint((field << 3) | 2), ...varint(payload.length), ...payload];
const str = (field, s) => bytes(field, [...Buffer.from(s, 'utf8')]);
const int = (field, n) => [...varint(field << 3), ...varint(n)];
const packed = (field, ns) => bytes(field, ns.flatMap(varint));

function scipIndex() {
	const sym = 'rust-analyzer cargo sourcegull-search 0.1.0 chunks/locate().';
	const occurrence = (range, roles) => bytes(2, [...packed(1, range), ...str(2, sym), ...int(3, roles)]);
	const toolInfo = bytes(2, [...str(1, 'rust-analyzer'), ...str(2, '0.0.1')]);
	const metadata = bytes(1, [...toolInfo, ...str(3, 'file:///src')]);
	const doc = bytes(2, [...str(1, 'crates/search/src/chunks.rs'), ...occurrence([41, 7, 13], 1), ...str(4, 'rust')]);
	return Buffer.from([...metadata, ...doc]);
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
	const row = (name) => `tr[data-policy="${name}"]`;

	step('sign in');
	await page.goto(`${BASE}/sign-in`);
	await page.fill('#username', 'admin');
	await page.fill('#password', 'correct horse battery');
	await page.click('button[type=submit]');
	await page.waitForURL((url) => !url.pathname.startsWith('/sign-in'));

	step('the built-in policies');
	await page.goto(`${BASE}/site-admin/code-graph`);
	await page.waitForSelector(row('Default tip-of-branch retention policy'), { timeout: 15000 });
	const tip = await page.textContent(row('Default tip-of-branch retention policy'));
	if (!tip.includes('All branches') || !tip.includes('Kept 12 weeks') || !tip.includes('built-in')) fail(`tip: ${tip}`);
	const commits = await page.textContent(row('Default commit retention policy'));
	if (!commits.includes('Kept 1 week, every commit on the branch')) fail(`commits: ${commits}`);
	if (await page.$(`${row('Default tag retention policy')} button:has-text("Delete")`)) fail('a built-in policy can be deleted');
	// The ranking status line loads above the policies.
	await page.waitForSelector('[data-testid="ranking-status"]:not(:has-text("Loading"))', { timeout: 10000 });
	const ranking = await page.textContent('[data-testid="ranking-status"]');
	if (!/Ranking:\s*(no runs yet|[\d,]+ files? in|the last run failed|computing)/.test(ranking)) fail(`ranking: ${ranking}`);
	await page.screenshot({ path: `${OUT}/policies-01-site-admin.png` });

	step('a built-in policy keeps its name');
	await page.click(`${row('Default tag retention policy')} button:has-text("Edit")`);
	if (!(await page.isDisabled('.editor input:not([type])'))) fail('the name of a built-in policy is editable');
	await page.click('.editor button:has-text("Cancel")');

	step('create a policy for a repository pattern');
	await page.click('button:has-text("New policy")');
	await page.fill('.editor input:not([type])', 'Release tags');
	await page.fill('.editor textarea', 'gitlab.example.com/acme/*');
	await page.selectOption('.editor select', 'GIT_TAG');
	await page.fill('.editor input.mono', 'v*');
	await page.waitForSelector('.preview:has-text("Matches every repository.")', { timeout: 10000 });
	await page.fill('.editor input.amount >> nth=0', '2');
	await page.selectOption('.editor fieldset >> nth=0 >> select', { label: 'days' });
	await page.screenshot({ path: `${OUT}/policies-02-editor.png` });
	await page.click('button:has-text("Create policy")');
	await page.waitForSelector(row('Release tags'), { timeout: 10000 });
	const created = await page.textContent(row('Release tags'));
	if (!created.includes('gitlab.example.com/acme/*') || !created.includes('Tags matching v*') || !created.includes('Kept 2 days'))
		fail(`created: ${created}`);

	step('edit it');
	await page.click(`${row('Release tags')} button:has-text("Edit")`);
	await page.fill('.editor input.amount >> nth=0', '');
	await page.click('button:has-text("Save")');
	await page.waitForSelector(`${row('Release tags')}:has-text("Kept forever")`, { timeout: 10000 });

	step('an invalid pattern is refused');
	await page.click('button:has-text("New policy")');
	await page.fill('.editor input:not([type])', 'Broken');
	await page.fill('.editor input.mono', 'release/[');
	await page.click('button:has-text("Create policy")');
	await page.waitForSelector('.editor .alert-danger:has-text("invalid glob pattern")', { timeout: 10000 });
	await page.click('.editor button:has-text("Cancel")');

	step('delete it');
	await page.click(`${row('Release tags')} button:has-text("Delete")`);
	await page.waitForSelector(row('Release tags'), { state: 'detached', timeout: 10000 });

	step('the repository page lists its policies and creates one');
	await page.goto(`${BASE}/${REPO}/-/code-graph`);
	await page.click('[role=tab]:has-text("Configuration policies")');
	await page.waitForSelector(row('Default tag retention policy'), { timeout: 15000 });
	await page.click('button:has-text("New policy for this repository")');
	await page.fill('.editor input:not([type])', 'Main forever');
	await page.fill('.editor input.mono', 'main');
	await page.waitForSelector('.preview:has-text("Matches 1 branch:")', { timeout: 10000 });
	const preview = await page.textContent('.preview');
	if (!preview.includes('main (')) fail(`preview: ${preview}`);
	await page.fill('.editor input.amount >> nth=0', '');
	await page.click('button:has-text("Create policy")');
	await page.waitForSelector(row('Main forever'), { timeout: 10000 });
	const mine = await page.textContent(row('Main forever'));
	if (!mine.includes(REPO) || !mine.includes('Branches matching main') || !mine.includes('Kept forever')) fail(`mine: ${mine}`);
	await page.screenshot({ path: `${OUT}/policies-03-repository.png` });

	step('an index at the head is kept by the tip of the default branch');
	const head = (await graphql(`query($r: String!) { repository(name: $r) { commit(rev: "HEAD") { oid } } }`, { r: REPO }))
		.repository.commit.oid;
	const params = new URLSearchParams({ repository: REPO, commit: head, root: 'crates/policies-e2e' });
	const r = await page.request.post(`${BASE}/.api/scip/upload?${params}`, {
		headers: { ...HEADERS, 'Content-Type': 'application/x-protobuf' },
		data: scipIndex()
	});
	if (r.status() !== 202) fail(`upload: ${r.status()} ${await r.text()}`);
	const indexRow = 'tr[data-root="crates/policies-e2e/"]';
	for (let i = 0; ; i++) {
		await page.goto(`${BASE}/${REPO}/-/code-graph`);
		await page.waitForSelector(indexRow, { timeout: 15000 });
		if ((await page.textContent(`${indexRow} .badge`)) === 'Completed') break;
		if (i > 60) fail(`not processed: ${await page.textContent(indexRow)}`);
		await page.waitForTimeout(500);
	}
	await page.click(`${indexRow} + tr.detail button:has-text("Retention")`);
	await page.waitForSelector('.retention:has-text("Tip of the default branch")', { timeout: 15000 });
	const retention = await page.textContent('.retention');
	if (!retention.includes('Main forever')) fail(`retention: ${retention}`);
	await page.screenshot({ path: `${OUT}/policies-04-retention.png` });

	step('phone layout');
	await page.setViewportSize({ width: 390, height: 844 });
	await page.goto(`${BASE}/site-admin/code-graph`);
	await page.waitForSelector(row('Default tag retention policy'));
	await page.click('button:has-text("New policy")');
	const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
	if (overflow > 1) fail(`horizontal page scroll on a phone: ${overflow}px`);
	await page.screenshot({ path: `${OUT}/policies-05-phone.png`, fullPage: true });

	if (problems.length) fail(problems.join('\n'));
	await browser.close();
	console.log('ok');
})().catch((e) => {
	console.error(e);
	process.exit(1);
});
