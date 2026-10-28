// Browser check of the repository's "Code graph data" page: uploads a
// small SCIP index through `POST /.api/scip/upload`, waits until the worker has processed it
// (which checks the commit against the repo node), then lists, reindexes and deletes it, and
// an upload for an unknown commit shows its error. Fails on any page or console error.
// Screenshots go to OUT.
//
// Run it with scripts/test/e2e-ui.sh, after ui/e2e/search.cjs has signed up the admin.
const { chromium } = require('playwright');
const BASE = process.env.BASE || 'http://127.0.0.1:7090';
const OUT = process.env.OUT || require('node:os').tmpdir();
const REPO = 'gitlab.example.com/acme/sourcegull';
const PAGE = `${BASE}/${REPO}/-/code-graph`;
const HEADERS = { 'X-Requested-With': 'Sourcegull' };

function step(name) {
	console.log(`- ${name}`);
}

// Protobuf encoding, just enough for a SCIP index.
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

/** A SCIP index of one document with a definition and a reference. */
function scipIndex() {
	const sym = 'rust-analyzer cargo sourcegull-search 0.1.0 chunks/locate().';
	const occurrence = (range, roles) => bytes(2, [...packed(1, range), ...str(2, sym), ...int(3, roles)]);
	const toolInfo = bytes(2, [...str(1, 'rust-analyzer'), ...str(2, '0.0.1')]);
	const metadata = bytes(1, [...toolInfo, ...str(3, 'file:///src')]);
	const doc = bytes(2, [
		...str(1, 'crates/search/src/chunks.rs'),
		...occurrence([41, 7, 13], 1),
		...occurrence([214, 16, 22], 0),
		...str(4, 'rust')
	]);
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
	const upload = async (commit) => {
		const params = new URLSearchParams({ repository: REPO, commit, root: 'crates/search' });
		const r = await page.request.post(`${BASE}/.api/scip/upload?${params}`, {
			headers: { ...HEADERS, 'Content-Type': 'application/x-protobuf' },
			data: scipIndex()
		});
		if (r.status() !== 202) fail(`upload: ${r.status()} ${await r.text()}`);
		return (await r.json()).id;
	};
	const rowState = (root) => page.textContent(`tr[data-root="${root}"] .badge`);

	step('sign in');
	await page.goto(`${BASE}/sign-in`);
	await page.fill('#username', 'admin');
	await page.fill('#password', 'correct horse battery');
	await page.click('button[type=submit]');
	await page.waitForURL((url) => !url.pathname.startsWith('/sign-in'));

	step('an empty page');
	await page.goto(PAGE);
	await page.waitForSelector('text=No precise indexes.', { timeout: 15000 });
	const tab = await page.getAttribute('.tabs a:has-text("Code graph")', 'aria-current');
	if (tab !== 'page') fail(`the Code graph tab is not current: ${tab}`);

	step('upload an index and wait for processing');
	const head = (await graphql(`query($r: String!) { repository(name: $r) { commit(rev: "HEAD") { oid } } }`, { r: REPO }))
		.repository.commit.oid;
	const id = await upload(head);
	await page.reload();
	await page.waitForSelector('tr[data-root="crates/search/"]', { timeout: 15000 }).catch(async (e) => {
		const data = await graphql('query { preciseIndexes(first: 5) { nodes { inputRoot state failure } } }');
		fail(`upload ${id} not listed: ${JSON.stringify(data)}\n${await page.textContent('main')}\n${e}`);
	});
	for (let i = 0; (await rowState('crates/search/')) !== 'Completed'; i++) {
		if (i > 60) fail(`not processed: ${await rowState('crates/search/')}`);
		await page.waitForTimeout(500);
		await page.reload();
		await page.waitForSelector('tr[data-root="crates/search/"]');
	}
	const row = await page.textContent('tr[data-root="crates/search/"]');
	if (!row.includes('rust-analyzer') || !row.includes(head.slice(0, 7))) fail(`row: ${row}`);
	const detail = await page.textContent('tr.detail');
	if (!detail.includes('1 documents · 2 occurrences')) fail(`detail: ${detail}`);
	await page.screenshot({ path: `${OUT}/codegraph-01-completed.png` });

	step('the root links to the directory at the commit');
	await page.click('a:text-is("crates/search/")');
	await page.waitForURL(new RegExp(`@${head}/-/tree/crates/search$`), { timeout: 10000 });
	await page.goBack();

	step('reindex');
	await page.waitForSelector('button:has-text("Reindex")');
	await page.click('button:has-text("Reindex")');
	await page.waitForSelector('[role=status]:has-text("Queued")', { timeout: 10000 });
	for (let i = 0; (await rowState('crates/search/')) !== 'Completed'; i++) {
		if (i > 60) fail(`not reprocessed: ${await rowState('crates/search/')}`);
		await page.waitForTimeout(500);
		await page.reload();
		await page.waitForSelector('tr[data-root="crates/search/"]');
	}

	step('an unknown commit shows its error');
	await upload('0'.repeat(40));
	await page.reload();
	await page.waitForSelector('.failure', { timeout: 30000 }).catch(async () => {
		for (let i = 0; i < 40 && !(await page.$('.failure')); i++) {
			await page.waitForTimeout(500);
			await page.reload();
		}
	});
	const failure = await page.textContent('.failure');
	if (!failure.includes('was not found in the repository')) fail(`failure: ${failure}`);
	await page.screenshot({ path: `${OUT}/codegraph-02-error.png` });

	step('filter by state');
	await page.selectOption('#index-state', { label: 'Completed' });
	await page.waitForFunction(() => document.querySelectorAll('tbody tr:not(.detail)').length === 1);

	step('delete');
	await page.click('button:has-text("Delete")');
	await page.waitForSelector('[role=status]:has-text("Deleting")', { timeout: 10000 });
	await page.waitForSelector('text=No precise indexes.', { timeout: 10000 });

	step('phone layout');
	await page.setViewportSize({ width: 390, height: 844 });
	await page.selectOption('#index-state', { label: 'All' });
	await page.waitForSelector('.failure');
	const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
	if (overflow > 1) fail(`horizontal page scroll on a phone: ${overflow}px`);
	await page.screenshot({ path: `${OUT}/codegraph-03-phone.png` });

	if (problems.length) fail(problems.join('\n'));
	await browser.close();
	console.log('ok');
})().catch((e) => {
	console.error(e);
	process.exit(1);
});
