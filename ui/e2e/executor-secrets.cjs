// Browser check of executor secrets: the site admin page creates an
// auto-indexing secret, never shows its value, replaces the value, shows the (empty) access
// logs, rejects an invalid DOCKER_AUTH_CONFIG and deletes the secret; a bulk changes secret
// is global; the user settings page lists it read-only and adds the user's own secret, which
// overwrites it. Fails on any page or console error. Screenshots go to OUT.
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
	const row = (key) => `tr[data-secret="${key}"]`;
	const add = async (key, value) => {
		await page.click('button:has-text("Add secret")');
		await page.fill('#secret-key', key);
		await page.fill('#secret-value', value);
		await page.click('form.create button[type=submit]');
	};

	step('sign in');
	await page.goto(`${BASE}/sign-in`);
	await page.fill('#username', 'admin');
	await page.fill('#password', 'correct horse battery');
	await page.click('button[type=submit]');
	await page.waitForURL((url) => !url.pathname.startsWith('/sign-in'));

	step('create an auto-indexing secret');
	await page.goto(`${BASE}/site-admin/executor-secrets`);
	await page.waitForSelector('td:has-text("No executor secrets.")', { timeout: 15000 });
	await add('NPM_TOKEN', 'e2e-secret-value-1');
	await page.waitForSelector(`${row('NPM_TOKEN')}, .alert-danger`);
	const alert = await page.$('.alert-danger');
	if (alert) fail(`creating: ${await alert.textContent()}`);
	const created = await page.textContent(row('NPM_TOKEN'));
	if (!created.includes('admin')) fail(`creator: ${created}`);
	if ((await page.content()).includes('e2e-secret-value-1')) fail('the value is shown');
	await page.screenshot({ path: `${OUT}/executor-secrets-01-list.png` });

	step('replace the value, access logs');
	await page.click(`${row('NPM_TOKEN')} button:has-text("Update value")`);
	await page.fill('#secret-new-value', 'e2e-secret-value-2');
	await page.click('form.update button[type=submit]');
	await page.waitForSelector('#secret-new-value', { state: 'detached' });
	await page.click(`${row('NPM_TOKEN')} button:has-text("Access logs")`);
	await page.waitForSelector('section.logs:has-text("The value has not been read yet.")');

	step('invalid DOCKER_AUTH_CONFIG');
	await add('DOCKER_AUTH_CONFIG', '{"credsStore": "desktop"}');
	await page.waitForSelector('.alert-danger:has-text("cannot use credential stores")');
	await page.click('form.create button:has-text("Cancel")');
	await page.screenshot({ path: `${OUT}/executor-secrets-02-invalid.png` });

	step('delete');
	await page.click(`${row('NPM_TOKEN')} button:has-text("Delete")`);
	await page.waitForSelector(row('NPM_TOKEN'), { state: 'detached' });

	step('a global bulk changes secret');
	await page.click('button[role=tab]:has-text("Bulk changes")');
	await page.waitForSelector('td:has-text("No executor secrets.")');
	await add('GITHUB_TOKEN', 'e2e-global-value');
	await page.waitForSelector(row('GITHUB_TOKEN'));

	step('user settings: global read-only, own secret overwrites it');
	await page.goto(`${BASE}/user/settings/executor-secrets`);
	await page.waitForSelector(row('GITHUB_TOKEN'));
	const global = await page.textContent(row('GITHUB_TOKEN'));
	if (!global.includes('global')) fail(`global badge: ${global}`);
	if (await page.$(`${row('GITHUB_TOKEN')} button:has-text("Delete")`)) fail('a global secret is editable here');
	await add('GITHUB_TOKEN', 'e2e-user-value');
	await page.waitForSelector(`${row('GITHUB_TOKEN')}:has-text("overwrites global")`);
	if (!(await page.$(`${row('GITHUB_TOKEN')} button:has-text("Delete")`))) fail('own secret not editable');
	await page.screenshot({ path: `${OUT}/executor-secrets-03-user.png` });

	step('phone layout');
	await page.setViewportSize({ width: 390, height: 844 });
	await page.goto(`${BASE}/site-admin/executor-secrets`);
	await page.waitForSelector('h1:has-text("Executor secrets")');
	const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
	if (overflow > 1) fail(`horizontal page scroll on a phone: ${overflow}px`);

	if (problems.length) fail(problems.join('\n'));
	await browser.close();
	console.log('ok');
})().catch((e) => {
	console.error(e);
	process.exit(1);
});
