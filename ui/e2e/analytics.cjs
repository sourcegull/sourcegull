// Browser check of the site admin Analytics pages: the overview with its
// date range and grouping controls, charts and tables, the user activity list, and the phone
// layout. Fails on any page or console error. Screenshots go to OUT.
//
// Run it with scripts/test/e2e-ui.sh, after ui/e2e/search.cjs has signed up the admin.
const { chromium } = require('playwright');
const BASE = process.env.BASE || 'http://127.0.0.1:7090';
const OUT = process.env.OUT || require('node:os').tmpdir();

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
	const fail = (msg) => {
		throw new Error(msg);
	};

	await page.goto(`${BASE}/sign-in`);
	await page.fill('#username', 'admin');
	await page.fill('#password', 'correct horse battery');
	await page.click('button[type=submit]');
	await page.waitForURL((url) => !url.pathname.startsWith('/sign-in'));

	step('overview with charts');
	await page.goto(`${BASE}/site-admin/analytics`);
	await page.waitForSelector('h1:has-text("Analytics")');
	await page.waitForSelector('figure.chart figcaption:has-text("Searches")');
	await page.waitForSelector('h2:has-text("Code navigation")');
	if ((await page.locator('figure.chart').count()) < 10) fail('expected the feature charts');
	await page.screenshot({ path: `${OUT}/an01-overview.png`, fullPage: true });

	step('date range and weekly grouping');
	await page.click('button:has-text("Last 3 months")');
	await page.selectOption('select', 'WEEKLY');
	await page.waitForSelector('button[aria-pressed=true]:has-text("Last 3 months")');
	await page.waitForTimeout(500);
	const bars = await page.locator('figure.chart').first().locator('rect.hit').count();
	if (bars < 13) fail(`expected weekly buckets over 3 months, got ${bars}`);

	step('user activity');
	await page.click('a:has-text("User activity")');
	await page.waitForSelector('h1:has-text("User activity")');
	await page.waitForSelector('td a:has-text("admin")');
	await page.click('button:has-text("Events")');
	await page.selectOption('select', 'never');
	await page.waitForTimeout(500);
	await page.screenshot({ path: `${OUT}/an02-users.png`, fullPage: true });

	step('phone layout');
	await page.setViewportSize({ width: 390, height: 844 });
	for (const path of ['/site-admin/analytics', '/site-admin/analytics/users']) {
		await page.goto(`${BASE}${path}`);
		await page.waitForTimeout(800);
		const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
		if (overflow > 0) fail(`${path} overflows by ${overflow}px at 390px`);
	}
	await page.screenshot({ path: `${OUT}/an03-mobile.png`, fullPage: true });

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
