// Browser check of the site admin Updates page: upgrade readiness
// with no schema drift, a drift item with its fix statement after a manual schema change, and
// the phone layout. Fails on any page or console error. Screenshots go to OUT.
//
// Run it with scripts/test/e2e-ui.sh, after ui/e2e/search.cjs has signed up the admin; it
// changes the schema with psql on SOURCEGULL_DATABASE_URL (and changes it back).
const { execFileSync } = require('node:child_process');
const { chromium } = require('playwright');
const BASE = process.env.BASE || 'http://127.0.0.1:7090';
const OUT = process.env.OUT || require('node:os').tmpdir();

function step(name) {
	console.log(`- ${name}`);
}

function psql(sql) {
	execFileSync('psql', [process.env.SOURCEGULL_DATABASE_URL, '-qc', sql], { stdio: 'inherit' });
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

	step('no schema drift on a migrated database');
	await page.goto(`${BASE}/site-admin/updates`);
	await page.waitForSelector('[role=status]:has-text("No schema drift")');
	await page.waitForSelector('a:has-text("Updates")');
	await page.screenshot({ path: `${OUT}/u01-updates.png` });

	step('a manual schema change shows up as drift with its fix');
	psql('CREATE TABLE e2e_drift (id int)');
	try {
		await page.click('button:has-text("Check again")');
		await page.waitForSelector('[role=status]:has-text("Schema drift: 1 difference")');
		await page.waitForSelector('li:has-text("table \\"e2e_drift\\"") pre:has-text("DROP TABLE e2e_drift;")');
		await page.screenshot({ path: `${OUT}/u02-drift.png`, fullPage: true });

		step('phone layout');
		await page.setViewportSize({ width: 390, height: 844 });
		await page.waitForTimeout(300);
		await page.screenshot({ path: `${OUT}/u03-mobile.png`, fullPage: true });
		const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
		if (overflow > 0) fail(`/site-admin/updates overflows by ${overflow}px at 390px`);
	} finally {
		psql('DROP TABLE IF EXISTS e2e_drift');
	}
	await page.click('button:has-text("Check again")');
	await page.waitForSelector('[role=status]:has-text("No schema drift")');

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
