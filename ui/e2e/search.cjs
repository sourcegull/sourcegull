// Browser check of the search UI: sign-up, search, filters, diff search,
// aggregations, saved searches, contexts and the phone layout, against a running sourcegull with one
// repository (the Sourcegull sources). Fails on any page or console error. Screenshots go to OUT.
//
// Run it with scripts/test/e2e-ui.sh, which sets up the server and the repository.
// Needs Playwright with Chromium: `npm i playwright && npx playwright install chromium`
// (in any directory on NODE_PATH; it is not a dependency of the UI package).
const { chromium } = require('playwright');
const BASE = process.env.BASE || 'http://127.0.0.1:7090';
const OUT = process.env.OUT || require('node:os').tmpdir();

function step(name) {
	console.log(`- ${name}`);
}

(async () => {
	const browser = await chromium.launch({ executablePath: process.env.CHROMIUM_EXECUTABLE });
	const context = await browser.newContext({ viewport: { width: 1400, height: 900 }, bypassCSP: true });
	const page = await context.newPage(); // waitForFunction evaluates in the page.
	const problems = [];
	page.on('pageerror', (e) => problems.push(`pageerror: ${e.message}`));
	page.on('console', (m) => {
		if (m.type() === 'error') problems.push(`console: ${m.text()}`);
	});
	page.on('dialog', (d) => d.accept());
	const fail = (msg) => {
		throw new Error(msg);
	};

	step('sign up the first user');
	await page.goto(`${BASE}/sign-up`);
	await page.fill('#username', 'admin');
	await page.fill('#email', 'admin@example.com');
	await page.fill('#password', 'correct horse battery');
	await page.click('button[type=submit]');
	await page.waitForURL(/\/(?:site-admin\/configuration|search)/);
	await page.goto(`${BASE}/search`);

	step('home page with the query box');
	await page.waitForSelector('.query-input .cm-content');
	await page.waitForTimeout(500);
	await page.screenshot({ path: `${OUT}/01-home.png` });

	step('highlighting and autocomplete');
	await page.click('.query-input .cm-content');
	await page.keyboard.type('lan');
	await page.waitForSelector('.cm-tooltip-autocomplete li', { timeout: 5000 });
	const options = await page.$$eval('.cm-tooltip-autocomplete li', (els) => els.map((e) => e.textContent));
	if (!options.some((o) => o.includes('lang:'))) fail(`no lang: completion: ${options}`);
	await page.keyboard.press('Enter'); // accepts "lang:" and opens value completion
	await page.waitForSelector('.cm-tooltip-autocomplete li', { timeout: 5000 });
	await page.keyboard.type('rus');
	await page.waitForFunction(() => {
		const options = [...document.querySelectorAll('.cm-tooltip-autocomplete li')];
		return options.length === 1 && options[0].textContent?.includes('rust');
	}, null, { timeout: 5000 });
	await page.keyboard.press('Enter');
	await page.keyboard.type('fn commit_log');
	const fieldMarks = await page.$$eval('.cm-q-field', (els) => els.map((e) => e.textContent));
	if (!fieldMarks.includes('lang:')) {
		const value = await page.textContent('.query-input .cm-content');
		await page.screenshot({ path: `${OUT}/02-typing-failed.png` });
		fail(`no field highlight: ${fieldMarks}; editor: ${value}`);
	}
	await page.screenshot({ path: `${OUT}/02-typing.png` });

	step('run the search');
	await page.keyboard.press('Enter');
	await page.waitForURL(/q=/);
	await page.waitForSelector('.result', { timeout: 20000 });
	await page.waitForFunction(() => !document.querySelector('.spinner'), null, { timeout: 30000 });
	const status = await page.textContent('.status [role=status]');
	console.log(`  status: ${status.replace(/\s+/g, ' ').trim()}`);
	const n = await page.$$eval('.result', (els) => els.length);
	if (n === 0) fail('no results');
	const marks = await page.$$eval('.result mark', (els) => els.length);
	if (marks === 0) fail('no highlighted matches');
	console.log(`  url: ${page.url()}`);
	if (!page.url().includes('context%3Aglobal')) fail('context not in the URL');
	await page.screenshot({ path: `${OUT}/03-results.png`, fullPage: false });

	step('keyboard navigation');
	await page.click('h2'); // focus out of the editor
	await page.keyboard.press('j');
	const focused = await page.evaluate(() => document.activeElement?.className ?? '');
	if (!focused.includes('result-header')) fail(`j did not focus a result: ${focused}`);
	await page.keyboard.press('/');
	const inEditor = await page.evaluate(() => !!document.activeElement?.closest('.query-input'));
	if (!inEditor) fail('/ did not focus the query box');

	step('result type filter: commits');
	await page.click('nav[aria-label="Search filters"] button:has-text("Commits")');
	await page.waitForURL(/type%3Acommit/);
	await page.waitForFunction(() => !document.querySelector('.spinner'), null, { timeout: 30000 });
	await page.screenshot({ path: `${OUT}/04-commits.png` });

	step('diff search');
	await page.goto(`${BASE}/search?q=${encodeURIComponent('context:global type:diff commit log')}`);
	await page.waitForSelector('.result', { timeout: 30000 });
	await page.waitForFunction(() => !document.querySelector('.spinner'), null, { timeout: 30000 });
	const diffRows = await page.$$eval('tr.added, tr.deleted', (els) => els.length);
	if (diffRows === 0) fail('no diff lines');
	await page.screenshot({ path: `${OUT}/05-diff.png` });

	step('language facet');
	await page.goto(`${BASE}/search?q=${encodeURIComponent('context:global CommitLog')}`);
	await page.waitForSelector('.result', { timeout: 30000 });
	await page.waitForFunction(() => !document.querySelector('.spinner'), null, { timeout: 30000 });
	await page.waitForSelector('nav[aria-label="Search filters"] h2:has-text("Languages")', { timeout: 10000 });
	await page.click('nav[aria-label="Search filters"] section:has(h2:has-text("Languages")) button.filter >> nth=0');
	await page.waitForURL(/lang%3A/);

	step('aggregations');
	await page.click('button:has-text("Group by")');
	await page.waitForSelector('.aggregations .bar-row, .aggregations p.muted', { timeout: 30000 });
	await page.waitForFunction(() => !document.querySelector('.aggregations[aria-busy="true"]'), null, { timeout: 30000 });
	await page.screenshot({ path: `${OUT}/06-aggregations.png` });

	step('diagnostics for an invalid query');
	await page.goto(`${BASE}/search`);
	await page.click('.query-input .cm-content');
	await page.keyboard.type('case:maybe foo');
	await page.waitForSelector('.diagnostic', { timeout: 5000 });
	console.log(`  diagnostic: ${await page.textContent('.diagnostic')}`);

	step('save a search');
	await page.goto(`${BASE}/search?q=${encodeURIComponent('context:global CommitLog')}`);
	await page.waitForSelector('.result', { timeout: 30000 });
	await page.click('button:has-text("Save search")');
	await page.fill('#save-description', 'Commit log uses');
	await page.click('form.save button[type=submit]');
	await page.waitForSelector('.alert-success', { timeout: 10000 });
	await page.click('.alert-success a:has-text("Open the saved search")');
	await page.waitForURL(/\/saved-searches\//);
	await page.waitForSelector('h1:has-text("Commit log uses")');
	await page.click('button:has-text("Edit")');
	await page.fill('#ss-description', 'Commit log uses (edited)');
	await page.click('button:has-text("Save changes")');
	await page.waitForSelector('h1:has-text("(edited)")');
	await page.click('button:has-text("Make public")');
	await page.waitForSelector('.badge:has-text("public")', { timeout: 10000 });
	await page.screenshot({ path: `${OUT}/07-saved-search.png` });
	await page.goto(`${BASE}/saved-searches`);
	await page.waitForSelector('.list a:has-text("(edited)")');
	await page.screenshot({ path: `${OUT}/08-saved-library.png` });
	await page.click('.list a.btn:has-text("Run")');
	await page.waitForURL(/\/search\?q=/);
	if (!page.url().includes('patternType') && !page.url().includes('q=')) fail('run link broken');

	step('create, star, default, edit and delete a context');
	await page.goto(`${BASE}/contexts/new`);
	await page.fill('#ctx-name', 'sourcegull-rust');
	await page.fill('#ctx-description', 'Rust code of Sourcegull');
	await page.click('.context-form .query-input .cm-content');
	await page.keyboard.type('repo:sourcegull lang:rust');
	await page.click('button:has-text("Create context")');
	await page.waitForURL(/\/contexts$/);
	await page.waitForSelector('a:has-text("@admin/sourcegull-rust")');
	await page.click('tr:has-text("@admin/sourcegull-rust") button.star');
	await page.waitForSelector('tr:has-text("@admin/sourcegull-rust") button.star.on');
	await page.click('tr:has-text("@admin/sourcegull-rust") button:has-text("Make default")');
	await page.waitForSelector('tr:has-text("@admin/sourcegull-rust") .badge:has-text("default")');
	await page.screenshot({ path: `${OUT}/09-contexts.png` });

	step('the default context is preselected and the picker lists contexts');
	await page.goto(`${BASE}/search`);
	await page.waitForSelector('.picker .ctx-spec:has-text("@admin/sourcegull-rust")', { timeout: 10000 });
	await page.click('.picker .trigger');
	await page.waitForSelector('.popover li[role=option]');
	await page.screenshot({ path: `${OUT}/10-picker.png` });
	await page.click('.popover li[role=option] button:has-text("global")');
	await page.waitForSelector('.picker .ctx-spec:has-text("global")');
	await page.click('.query-input .cm-content');
	await page.keyboard.type('context:@adm');
	await page.waitForSelector('.cm-tooltip-autocomplete li:has-text("@admin/sourcegull-rust")', { timeout: 5000 });

	step('search in the context');
	await page.goto(`${BASE}/search?q=${encodeURIComponent('context:@admin/sourcegull-rust CommitLog')}`);
	await page.waitForSelector('.result', { timeout: 30000 });
	const langs = await page.$$eval('.result .meta', (els) => [...new Set(els.map((e) => e.textContent.trim()))]);
	console.log(`  languages in context results: ${langs}`);
	if (langs.some((l) => l && l !== 'Rust')) fail(`context did not apply lang:rust: ${langs}`);

	step('edit and delete the context');
	await page.goto(`${BASE}/contexts`);
	await page.click('tr:has-text("@admin/sourcegull-rust") a:has-text("Edit")');
	await page.waitForSelector('#ctx-description');
	await page.fill('#ctx-description', 'Edited');
	await page.click('button:has-text("Save changes")');
	await page.waitForURL(/\/contexts$/);
	await page.waitForSelector('tr:has-text("@admin/sourcegull-rust") .desc:has-text("Edited")');
	await page.click('tr:has-text("@admin/sourcegull-rust") button:has-text("Delete")');
	await page.waitForSelector('a:has-text("@admin/sourcegull-rust")', { state: 'detached' });

	step('mobile layout');
	await page.setViewportSize({ width: 390, height: 844 });
	await page.goto(`${BASE}/search?q=${encodeURIComponent('context:global CommitLog')}`);
	await page.waitForSelector('.result', { timeout: 30000 });
	const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
	console.log(`  horizontal overflow at 390px: ${overflow}px`);
	if (overflow > 0) {
		const wide = await page.$$eval('*', (elements) =>
			elements
				.map((element) => ({ tag: element.tagName, className: element.className, right: element.getBoundingClientRect().right }))
				.filter((element) => element.right > window.innerWidth + 1)
				.sort((a, b) => b.right - a.right)
				.slice(0, 8)
		);
		console.log('  widest elements:', wide);
		fail(`mobile layout overflows by ${overflow}px`);
	}
	await page.screenshot({ path: `${OUT}/11-mobile.png` });

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
