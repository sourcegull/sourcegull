// Browser check of a watch's result filtering: the form names whom the watch runs as and
// says that each email recipient gets only the results they can read; a site admin can turn
// on unfiltered results, which shows the red warning and needs the acknowledgement; the saved
// watch shows the unfiltered badge and who turned it on. Fails on any page or console error.
// Screenshots go to OUT.
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
	const context = await browser.newContext({ viewport: { width: 1400, height: 1000 }, bypassCSP: true });
	const page = await context.newPage();
	const problems = [];
	page.on('pageerror', (e) => problems.push(`pageerror: ${e.message}`));
	page.on('console', (m) => {
		if (m.type() === 'error') problems.push(`console: ${m.text()}`);
	});
	const fail = (msg) => {
		throw new Error(msg);
	};

	step('sign in');
	await page.goto(`${BASE}/sign-in`);
	await page.fill('#username', 'admin');
	await page.fill('#password', 'correct horse battery');
	await page.click('button[type=submit]');
	await page.waitForURL((url) => !url.pathname.startsWith('/sign-in'));

	step('the form explains whose results recipients get');
	await page.goto(`${BASE}/watches/new`);
	await page.waitForSelector('legend:has-text("Who sees which results")', { timeout: 15000 });
	const section = await page.textContent('fieldset:has(legend:has-text("Who sees which results"))');
	if (!section.includes('repository permissions of @admin')) fail(`runs as: ${section}`);
	if (!section.includes('only the results in repositories they can read')) fail(`filtering: ${section}`);
	if (await page.isVisible('text=Warning: this bypasses repository permissions')) fail('warning shown while filtered');

	step('turning on unfiltered results warns and needs the acknowledgement');
	await page.fill('#cm-description', 'Unfiltered e2e');
	await page.check('label:has-text("Send unfiltered results to every email recipient") input');
	await page.waitForSelector('text=Warning: this bypasses repository permissions');
	await page.screenshot({ path: `${OUT}/watches-01-unfiltered-warning.png`, fullPage: true });
	await page.click('button:has-text("Create watch")');
	if (!page.url().endsWith('/watches/new')) fail('created without the acknowledgement');
	await page.check('label:has-text("I understand") input');
	await page.click('button:has-text("Create watch")');
	await page.waitForURL((url) => /\/watches\/[^/]+$/.test(url.pathname) && !url.pathname.endsWith('/new'), {
		timeout: 15000
	});

	step('the saved watch shows who made it unfiltered');
	await page.waitForSelector('.badge-danger:has-text("unfiltered")', { timeout: 15000 });
	const header = await page.textContent('.page-header');
	if (!header.includes('runs as @admin')) fail(`header: ${header}`);
	const alert = await page.textContent('.alert-danger:has-text("Unfiltered:")');
	if (!alert.includes('Turned on by @admin')) fail(`alert: ${alert}`);
	if (!(await page.isChecked('label:has-text("Send unfiltered results to every email recipient") input')))
		fail('the toggle is off on the saved watch');
	await page.screenshot({ path: `${OUT}/watches-02-saved.png`, fullPage: true });

	await browser.close();
	if (problems.length) fail(problems.join('\n'));
	console.log('watches: ok');
})().catch((e) => {
	console.error(e);
	process.exit(1);
});
