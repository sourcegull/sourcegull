// Browser check of the site admin repository pages: the repository list
// with its status filters, a repository's mirror and text search index status with the
// re-index action, the repo nodes page with disk usage, and the phone layout of each page.
// Fails on any page or console error. Screenshots go to OUT.
//
// Run it with scripts/test/e2e-ui.sh, after ui/e2e/search.cjs has signed up the admin and with
// the repository it sets up.
const { chromium } = require('playwright');
const BASE = process.env.BASE || 'http://127.0.0.1:7090';
const OUT = process.env.OUT || require('node:os').tmpdir();
const REPO = 'gitlab.example.com/acme/sourcegull';

function step(name) {
	console.log(`- ${name}`);
}

(async () => {
	const browser = await chromium.launch({ executablePath: process.env.CHROMIUM_EXECUTABLE });
	const problems = [];
	const context = await browser.newContext({ viewport: { width: 1400, height: 900 }, bypassCSP: true });
	const page = await context.newPage();
	page.on('pageerror', (e) => problems.push(`pageerror: ${e.message}`));
	page.on('console', (m) => {
		if (m.type() === 'error') problems.push(`console: ${m.text()}`);
	});
	page.on('dialog', (d) => d.accept());
	const fail = (msg) => {
		throw new Error(msg);
	};
	const row = `tr:has(a:text-is("${REPO}"))`;

	await page.goto(`${BASE}/sign-in`);
	await page.fill('#username', 'admin');
	await page.fill('#password', 'correct horse battery');
	await page.click('button[type=submit]');
	await page.waitForURL((url) => !url.pathname.startsWith('/sign-in'));

	step('the repository list shows the cloned repository');
	await page.goto(`${BASE}/site-admin/repositories`);
	await page.waitForSelector(`${row} .badge:has-text("cloned")`);
	await page.screenshot({ path: `${OUT}/r01-repositories.png` });

	step('filters: corrupted, not indexed, search');
	await page.click('[aria-label="Filter repositories"] button:text-matches("^Corrupted")');
	await page.waitForSelector('td:has-text("No repositories match.")');
	await page.click('[aria-label="Filter repositories"] button:text-matches("^Not indexed")');
	await page.waitForSelector(row);
	await page.click('[aria-label="Filter repositories"] button:text-matches("^All")');
	await page.fill('input[type=search]', 'acme/sOURCEgull');
	await page.click('button:has-text("Search")');
	await page.waitForSelector(row);

	step('the repository page shows the mirror; re-index builds the index of HEAD');
	await page.click(`${row} a`);
	await page.waitForURL(/\/site-admin\/repositories\/gitlab\.example\.com\/acme\/sourcegull$/);
	await page.waitForSelector('h2:has-text("Text search index")');
	await page.waitForSelector('dd:has-text("cloned")');
	await page.click('button:has-text("Re-index")');
	await page.waitForSelector('[role=status]:has-text("Reindex queued")');
	for (let i = 0; ; i++) {
		if (await page.$('tr:has-text("(default)") .badge:has-text("current")')) break;
		if (i >= 120) fail('HEAD never showed up as indexed');
		await page.waitForTimeout(1000);
		await page.reload();
		await page.waitForSelector('h2:has-text("Text search index")');
	}
	await page.waitForSelector('td:has-text(".gullshard")');
	await page.screenshot({ path: `${OUT}/r02-repository.png`, fullPage: true });
	await page.click('button:has-text("Fetch now")');
	await page.waitForSelector('[role=status]:has-text("Fetch queued")');

	step('the indexed filter lists the repository');
	await page.goto(`${BASE}/site-admin/repositories`);
	await page.waitForSelector(row);
	await page.click('[aria-label="Filter repositories"] button:text-matches("^Indexed")');
	await page.waitForSelector('table[aria-busy=false]');
	await page.waitForSelector(row);

	step('the repo nodes page shows disk usage');
	await page.goto(`${BASE}/site-admin/repo-nodes`);
	await page.waitForSelector('[role=meter]');
	const held = await page.textContent('tbody tr td:nth-child(3)');
	if (!held.includes('1 held')) fail(`unexpected repository count: ${held}`);
	const summary = await page.textContent('[data-testid=rebalance-summary]');
	if (!summary.includes('Balanced')) fail(`unexpected rebalancing summary: ${summary}`);
	await page.screenshot({ path: `${OUT}/r03-repo-nodes.png` });

	step('phone layout');
	await page.setViewportSize({ width: 390, height: 844 });
	const paths = ['/site-admin/repositories', `/site-admin/repositories/${REPO}`, '/site-admin/repo-nodes'];
	for (const [i, path] of paths.entries()) {
		await page.goto(`${BASE}${path}`);
		await page.waitForSelector('h1');
		await page.waitForTimeout(500);
		await page.screenshot({ path: `${OUT}/r04-mobile-${i}.png`, fullPage: true });
		const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
		if (overflow > 0) fail(`${path} overflows by ${overflow}px at 390px`);
	}

	await browser.close();
	if (problems.length) {
		console.log('PROBLEMS:\n' + problems.join('\n'));
		process.exit(1);
	}
	console.log('OK');
})().catch(async (e) => {
	console.error('FAILED:', e.message);
	process.exit(1);
});
