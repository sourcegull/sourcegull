// Browser check of the code host connections admin pages: the settings form writes the
// configuration (groups, exclusions, permissions) and tests it before saving, the JSON editor
// validates the configuration live, a connection is added (pointing at a closed port, so the
// save warns and "test connection" reports the problem), the webhook URL and secret are shown
// and the secret regenerated, the next steps (sign-in, permissions) are listed, "sync now" adds a failed run with its error to the sync history,
// an edit keeps the redacted secrets, and the connection is deleted. Fails on any page or
// console error. Screenshots go to OUT.
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
		// The failing GraphQL validation responses are expected; anything else is not.
		if (m.type() === 'error' && !m.text().includes('status of 4')) problems.push(`console: ${m.text()}`);
	});
	page.on('dialog', (d) => d.accept());
	const fail = (msg) => {
		throw new Error(msg);
	};
	const setConfig = async (text) => {
		await page.click('.cm-content');
		await page.keyboard.press('Control+A');
		await page.keyboard.press('Delete');
		await page.keyboard.insertText(text);
	};
	const validation = '[data-testid=config-validation]';

	step('sign in');
	await page.goto(`${BASE}/sign-in`);
	await page.fill('#username', 'admin');
	await page.fill('#password', 'correct horse battery');
	await page.click('button[type=submit]');
	await page.waitForURL((url) => !url.pathname.startsWith('/sign-in'));

	step('empty list');
	await page.goto(`${BASE}/site-admin/external-services`);
	await page.waitForSelector('td:has-text("No code host connections yet.")', { timeout: 15000 });

	step('settings form');
	await page.click('a:has-text("Add connection")');
	await page.waitForSelector('#gl-url');
	await page.waitForSelector('.setup[open] a:has-text("Access tokens")');
	await page.fill('#gl-url', 'http://127.0.0.1:9');
	await page.fill('#gl-token-input', 'form-token');
	await page.check('input[name=gl-mode] >> nth=1');
	await page.fill('#gl-groups', 'http://127.0.0.1:9/acme/platform/\ntools');
	await page.fill('#gl-projects-input', 'other/api');
	await page.click('#gl-url');
	await page.click('summary:has-text("Exclusions")');
	await page.fill('#gl-exclude-patterns', '^acme/sandbox/');
	await page.click('#gl-url');
	if ((await page.inputValue('#gl-groups')) !== 'acme/platform\ntools') fail('group URL not normalized');
	await page.selectOption('#gl-auth', 'oauth');
	await page.waitForSelector(`${validation} :text("The configuration is valid.")`);
	await page.click('[data-testid=config-check] button:has-text("Test connection")');
	await page.waitForSelector('[data-testid=config-check] .check-bad:has-text("could not reach")');
	await page.screenshot({ path: `${OUT}/connections-00-form.png`, fullPage: true });
	await page.click('[role=tab]:has-text("Edit as JSON")');
	await page.waitForSelector('.cm-content:has-text("groups/acme%2Fplatform/projects")');
	for (const want of ['form-token', '"name": "other/api"', '"pattern": "^acme/sandbox/"', '"type": "oauth"']) {
		if (!(await page.textContent('.cm-content')).includes(want)) fail(`JSON lacks ${want}`);
	}

	step('live validation');
	await page.waitForSelector(`${validation} :text("The configuration is valid.")`);
	await setConfig('{"url": "https://gitlab.example.com", "token": "t", "nope": 1}');
	await page.waitForSelector(`${validation} .alert-danger:has-text("nope")`);
	await page.screenshot({ path: `${OUT}/connections-01-invalid.png` });

	step('add a connection');
	await setConfig(
		'{\n  // e2e\n  "url": "http://127.0.0.1:9",\n  "token": "e2e-token",\n  "projectQuery": ["projects?membership=true"],\n  "webhooks": [{"secret": "e2e-hook-secret"}]\n}'
	);
	await page.waitForSelector(`${validation} :text("The configuration is valid.")`);
	await page.fill('#connection-name', 'E2E GitLab');
	await page.click('button:has-text("Add connection")');
	await page.waitForURL(/\/site-admin\/external-services\/[^/]+$/);
	await page.waitForSelector('h1:has-text("E2E GitLab")');
	await page.waitForSelector('.alert-info:has-text("could not reach")');

	step('next steps: sign-in with the host and its repository permissions');
	await page.waitForSelector('#next-steps:has-text("Next steps")');
	await page.waitForSelector('.checklist li:has-text("Let people sign in with GitLab") a[href="/site-admin/configuration?section=auth"]');
	await page.waitForSelector('.checklist li:has-text("Use GitLab repository permissions") a:has-text("Edit connection")');
	await page.screenshot({ path: `${OUT}/connections-02-next-steps.png`, fullPage: true });

	step('test connection');
	await page.click('button:has-text("Test connection")');
	await page.waitForSelector('[data-testid=connection-check] .alert-danger:has-text("could not reach")');

	step('webhook URL and secret');
	const url = await page.textContent('[data-testid=webhook-url]');
	if (!url.includes('/.api/webhooks/')) fail(`webhook URL: ${url}`);
	if ((await page.textContent('[data-testid=webhook-secret]')).includes('e2e-hook-secret')) fail('stored secret shown');
	await page.click('button:has-text("Regenerate secret")');
	await page.waitForSelector('.alert-info:has-text("new webhook secret")');
	await page.waitForSelector('[data-testid=webhook-secret-once]');
	const secret = (await page.textContent('[data-testid=webhook-secret]')).trim();
	if (secret === 'e2e-hook-secret' || secret.length !== 43) fail(`regenerated secret: ${secret}`);
	await page.screenshot({ path: `${OUT}/connections-02-detail.png` });

	step('sync now: the run fails and the history shows why');
	await page.click('.page-header button:has-text("Sync now")');
	await page.waitForSelector('tr[data-job-state=FAILED], tr[data-job-state=ERRORED]', { timeout: 60000 });
	await page.waitForSelector('tr.failure .error-text');
	await page.waitForSelector('.last-error');
	await page.screenshot({ path: `${OUT}/connections-03-history.png`, fullPage: true });

	step('edit keeps the redacted secrets');
	await page.click('a.btn:has-text("Edit")');
	await page.waitForSelector('#gl-token-input[placeholder^="Stored token kept"]');
	if ((await page.inputValue('#gl-token-input')) !== '') fail('the stored token is in the form');
	await page.click('[data-testid=config-check] button:has-text("Test connection")');
	await page.waitForSelector('[data-testid=config-check] .check-bad:has-text("could not reach")');
	await page.click('[role=tab]:has-text("Edit as JSON")');
	await page.waitForSelector('.cm-content:has-text("REDACTED")');
	if ((await page.content()).includes('e2e-token')) fail('the token is shown in the editor');
	await page.fill('#connection-name', 'E2E GitLab renamed');
	await page.click('button[type=submit]:has-text("Save")');
	await page.waitForSelector('h1:has-text("E2E GitLab renamed")');
	await page.waitForSelector(`[data-testid=webhook-secret]:has-text("…${secret.slice(-4)}")`);
	if ((await page.content()).includes(secret)) fail('the regenerated secret is shown after leaving the page');

	step('phone layout');
	await page.setViewportSize({ width: 390, height: 844 });
	await page.reload();
	await page.waitForSelector('h1:has-text("E2E GitLab renamed")');
	const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
	if (overflow > 1) fail(`horizontal page scroll on a phone: ${overflow}px`);
	await page.setViewportSize({ width: 1400, height: 900 });

	step('list and delete');
	await page.goto(`${BASE}/site-admin/external-services`);
	await page.waitForSelector('tr[data-connection="E2E GitLab renamed"]');
	await page.click('tr[data-connection="E2E GitLab renamed"] a:has-text("E2E GitLab renamed")');
	await page.click('.page-header button:has-text("Delete")');
	await page.waitForURL(/\/site-admin\/external-services$/);
	await page.waitForSelector('td:has-text("No code host connections yet.")');

	if (problems.length) fail(problems.join('\n'));
	await browser.close();
	console.log('ok');
})().catch((e) => {
	console.error(e);
	process.exit(1);
});
