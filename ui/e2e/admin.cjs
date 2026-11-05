// Browser check of the identity and permissions admin pages: access
// requests (request, approve, set the password, sign in), creating a user (setup link, sign in),
// users and their roles, custom roles,
// related links between admin pages and their settings, the settings search, the overview's
// setup checklist, the sign-in choice of "Connect an agent", "why can X see Y", sync now and the
// job history, the global and organization settings forms, access tokens, and the phone layout of each page. Fails on any page or console
// error. Screenshots go to OUT.
//
// Run it with scripts/test/e2e-ui.sh, after ui/e2e/search.cjs has signed up the admin and with
// the repository it sets up.
const { chromium } = require('playwright');
const BASE = process.env.BASE || 'http://127.0.0.1:7090';
const OUT = process.env.OUT || require('node:os').tmpdir();
const REPO = 'gitlab.example.com/acme/sourcegull';
const ROLE = 'e2e-reviewers';

function step(name) {
	console.log(`- ${name}`);
}

(async () => {
	const browser = await chromium.launch({ executablePath: process.env.CHROMIUM_EXECUTABLE });
	const problems = [];
	const newPage = async () => {
		const context = await browser.newContext({ viewport: { width: 1400, height: 900 }, bypassCSP: true });
		const page = await context.newPage();
		page.on('pageerror', (e) => problems.push(`pageerror: ${e.message}`));
		page.on('console', (m) => {
			if (m.type() === 'error') problems.push(`console: ${m.text()}`);
		});
		page.on('dialog', (d) => d.accept());
		return page;
	};
	const fail = (msg) => {
		throw new Error(msg);
	};
	const signIn = async (page, username, password) => {
		await page.goto(`${BASE}/sign-in`);
		await page.fill('#username', username);
		await page.fill('#password', password);
		await page.click('button[type=submit]');
		await page.waitForURL((url) => !url.pathname.startsWith('/sign-in'));
	};

	step('request access while signed out');
	const guest = await newPage();
	await guest.goto(`${BASE}/sign-in`);
	await guest.click('a:has-text("Request access")');
	await guest.waitForURL(/\/request-access$/);
	await guest.fill('#name', 'Bob Builder');
	await guest.fill('#email', 'bob@example.com');
	await guest.fill('#info', 'Reviewing the search code.');
	await guest.click('button[type=submit]');
	await guest.waitForSelector('[role=status]:has-text("A site admin will review")');
	await guest.screenshot({ path: `${OUT}/a01-request-access.png` });

	step('approve it as the site admin');
	const admin = await newPage();
	await signIn(admin, 'admin', 'correct horse battery');
	await admin.goto(`${BASE}/site-admin/access-requests`);
	await admin.waitForSelector('tr:has-text("bob@example.com")');
	await admin.screenshot({ path: `${OUT}/a02-access-requests.png` });
	await admin.click('tr:has-text("bob@example.com") button:has-text("Approve")');
	const link = (await admin.textContent('.setup-link', { timeout: 10000 })).trim();
	if (!link.includes('/set-password')) fail(`unexpected setup link: ${link}`);
	await admin.waitForSelector('td:has-text("No pending access requests.")');

	step('set the password from the setup link and sign in');
	await guest.goto(link);
	await guest.fill('#password', 'bob builds things');
	await guest.click('button[type=submit]');
	await guest.waitForSelector('[role=status]:has-text("Your password is set")');
	await admin.goto(`${BASE}/site-admin/users`);
	await admin.fill('#user-filter', 'bob');
	await admin.waitForFunction(() => document.querySelectorAll('tbody tr').length === 1 && !document.querySelector('[aria-busy=true]'));
	const bob = (await admin.textContent('tbody tr strong')).trim();
	await signIn(guest, bob, 'bob builds things');

	step('create a user as the site admin, set the password from the link and sign in');
	await admin.goto(`${BASE}/site-admin/users`);
	await admin.fill('#new-username', 'carol');
	await admin.fill('#new-email', 'carol@example.com');
	await admin.click('button:has-text("Create user")');
	const created = '[role=status]:has-text("Created carol") .reset-link';
	const carolLink = (await admin.textContent(created, { timeout: 10000 })).trim();
	if (!carolLink.includes('/set-password?token=')) fail(`unexpected setup link: ${carolLink}`);
	await admin.waitForSelector('tbody strong:text-is("carol")');
	await admin.screenshot({ path: `${OUT}/a03b-create-user.png` });
	await admin.fill('#new-username', 'carol');
	await admin.click('button:has-text("Create user")');
	await admin.waitForSelector('[role=alert]:has-text("username is already taken")');
	const carolPage = await newPage();
	await carolPage.goto(carolLink);
	await carolPage.fill('#password', 'carol creates things');
	await carolPage.click('button[type=submit]');
	await carolPage.waitForSelector('[role=status]:has-text("Your password is set")');
	await signIn(carolPage, 'carol', 'carol creates things');
	await carolPage.context().close();

	step('create a custom role with a permission');
	await admin.goto(`${BASE}/site-admin/roles`);
	await admin.waitForSelector('.role-card');
	await admin.fill('#role-name', ROLE);
	await admin.click('button:has-text("Create role")');
	const card = `.role-card:has(h2:has-text("${ROLE}"))`;
	await admin.waitForSelector(card);
	const box = `${card} input[type=checkbox] >> nth=0`;
	if (await admin.isChecked(box)) fail('a new role starts with a permission');
	await admin.check(box);
	await admin.click(`${card} button:has-text("Save permissions")`);
	await admin.waitForSelector(`${card} button:has-text("Save permissions"):not([disabled])`);
	await admin.waitForTimeout(300);
	await admin.reload();
	await admin.waitForSelector(card);
	if (!(await admin.isChecked(box))) fail('the permission was not saved');
	await admin.screenshot({ path: `${OUT}/a03-roles.png`, fullPage: true });

	step('give the new user the role');
	await admin.goto(`${BASE}/site-admin/users`);
	const row = `tr:has(strong:text-is("${bob}"))`;
	await admin.click(`${row} button:has-text("Edit roles")`);
	await admin.check(`.role-editor label:has-text("${ROLE}") input`);
	await admin.click('.role-editor button:has-text("Save roles")');
	await admin.waitForSelector(`${row} .badge:has-text("${ROLE}")`);
	await admin.screenshot({ path: `${OUT}/a04-users.png` });

	step('global settings: forms keep comments, save, and show in the JSON view');
	await admin.goto(`${BASE}/site-admin/global-settings`);
	await admin.waitForSelector('#cfg-search_defaultPatternType');
	if (await admin.locator('#cfg-search_includeForks').count()) fail('a setting without effect is in the form');
	if (await admin.locator('#cfg-orgs_allMembersBatchChangesAdmin').count()) fail('an organization setting is in the global form');
	await admin.click('button[role=tab]:has-text("Edit as JSON")');
	await admin.waitForSelector('.cm-content');
	await admin.click('.cm-content');
	await admin.keyboard.press('Control+A');
	await admin.keyboard.insertText('{\n  // kept by the forms\n  "my.custom": 1\n}\n');
	await admin.click('button[role=tab]:has-text("Settings")');
	await admin.selectOption('#cfg-search_defaultPatternType', 'regexp');
	await admin.click('button:has-text("Add notice")');
	await admin.click('.settings-toolbar button:text-is("Save")');
	await admin.waitForSelector('#cfg-notices--0--message-error:has-text("Required.")');
	await admin.fill('#cfg-notices--0--message', 'First');
	await admin.click('button:has-text("Add notice")');
	await admin.fill('#cfg-notices--1--message', 'Second');
	await admin.selectOption('#cfg-notices--1--location', 'home');
	await admin.click('button[aria-label="Move Notice 2 up"]');
	if ((await admin.inputValue('#cfg-notices--0--message')) !== 'Second') fail('moving a notice did not reorder the list');
	await admin.click('button[aria-label="Remove Notice 2"]');
	await admin.click('button:has-text("Add quicklink")');
	await admin.fill('#cfg-quicklinks--0--name', 'Docs');
	await admin.fill('#cfg-quicklinks--0--url', '/help');
	await admin.click('.settings-toolbar button:text-is("Save")');
	await admin.waitForSelector('.alert-success:text-is("Saved.")');
	if (await admin.getAttribute('#cfg-quicklinks--0--name', 'aria-invalid')) fail('a filled-in value is still marked invalid');
	await admin.waitForTimeout(400);
	await admin.screenshot({ path: `${OUT}/a05-global-settings.png`, fullPage: true });
	await admin.reload();
	await admin.waitForSelector('#cfg-notices--0--message');
	if ((await admin.inputValue('#cfg-notices--0--message')) !== 'Second') fail('the notice was not saved');
	if ((await admin.inputValue('#cfg-notices--0--location')) !== 'home') fail('the notice location was not saved');
	if (!(await admin.textContent('.settings-section:last-of-type, #settings-panel')).includes('my.custom')) fail('an unknown key is not listed');
	await admin.click('button[role=tab]:has-text("Edit as JSON")');
	const globalJson = await admin.textContent('.cm-content');
	if (!globalJson.includes('kept by the forms') || !globalJson.includes('my.custom')) fail(`the forms lost text: ${globalJson}`);

	step('the saved settings take effect: pattern type, quicklinks, notices');
	await admin.goto(`${BASE}/search`);
	await admin.waitForFunction(() => document.querySelector('#pattern-type')?.value === 'regexp');
	await admin.waitForSelector('nav[aria-label="Quick links"] a[href="/help"]:has-text("Docs")');
	await admin.waitForSelector('[data-testid="notice-home"]:has-text("Second")');
	await admin.screenshot({ path: `${OUT}/a05b-search-home-settings.png`, fullPage: true });
	await admin.goto(`${BASE}/search?q=settings-link`);
	await admin.waitForTimeout(500);
	if ((await admin.inputValue('#pattern-type')) !== 'keyword') fail('the setting changed what a link searches for');
	await admin.goto(`${BASE}/site-admin/global-settings`);
	await admin.waitForSelector('input[name="patternType"][value="regexp"]', { state: 'attached' });

	step('notices are Markdown without unsafe links, and stay dismissed');
	await admin.click('button[role=tab]:has-text("Edit as JSON")');
	await admin.waitForSelector('.cm-content');
	await admin.click('.cm-content');
	await admin.keyboard.press('Control+A');
	await admin.keyboard.insertText(
		JSON.stringify({
			'search.defaultPatternType': 'regexp',
			quicklinks: [
				{ name: 'Docs', url: '/help' },
				{ name: 'Unsafe', url: 'java\tscript:alert(1)' }
			],
			notices: [{ message: '**Maintenance** [link](javascript:alert(1))', location: 'top', dismissible: true }]
		})
	);
	await admin.click('.settings-toolbar button:text-is("Save")');
	await admin.waitForSelector('.alert-success:text-is("Saved.")');
	await admin.waitForSelector('[data-testid="notice-top"] strong:has-text("Maintenance")');
	if (await admin.locator('[data-testid="notice-top"] a[href*="script"]').count()) fail('a notice rendered a script link');
	await admin.screenshot({ path: `${OUT}/a05c-top-notice.png` });
	await admin.click('[data-testid="notice-top"] button:has-text("Dismiss")');
	await admin.reload();
	await admin.waitForSelector('#cfg-search_defaultPatternType');
	await admin.waitForSelector('input[name="patternType"]', { state: 'attached' });
	if (await admin.locator('[data-testid="notice-top"]').count()) fail('a dismissed notice came back');
	await admin.goto(`${BASE}/search`);
	await admin.waitForSelector('nav[aria-label="Quick links"] a[href="/help"]');
	if (await admin.locator('nav[aria-label="Quick links"] a:has-text("Unsafe")').count()) fail('an unsafe quicklink is shown');

	step('user preferences: inherited values, and they override the global settings');
	await admin.goto(`${BASE}/user/settings/preferences`);
	await admin.waitForSelector('#cfg-search_defaultPatternType');
	if (!(await admin.textContent('#cfg-search_defaultPatternType option:first-child')).includes('regexp')) fail('the inherited value is not shown');
	if (await admin.locator('#cfg-orgs_allMembersBatchChangesAdmin, button:has-text("Add notice")').count()) fail('the user form shows settings for others');
	await admin.selectOption('#cfg-search_defaultPatternType', 'literal');
	await admin.click('.settings-toolbar button:text-is("Save")');
	await admin.waitForSelector('.alert-success:text-is("Saved.")');
	await admin.screenshot({ path: `${OUT}/a05d-preferences.png`, fullPage: true });
	await admin.goto(`${BASE}/search`);
	await admin.waitForFunction(() => document.querySelector('#pattern-type')?.value === 'literal');
	await admin.goto(`${BASE}/user/settings/preferences`);
	await admin.waitForSelector('#cfg-search_defaultPatternType');
	await admin.selectOption('#cfg-search_defaultPatternType', '');
	await admin.click('.settings-toolbar button:text-is("Save")');
	await admin.waitForSelector('.alert-success:text-is("Saved.")');

	step('organization settings: inherited values, and the organization-only setting');
	await admin.goto(`${BASE}/organizations/new`);
	await admin.fill('#name', 'e2e-settings');
	await admin.click('button[type=submit]');
	await admin.waitForURL(/organizations\/e2e-settings/);
	await admin.goto(`${BASE}/organizations/e2e-settings/settings`);
	await admin.waitForSelector('#cfg-orgs_allMembersBatchChangesAdmin');
	const inherited = await admin.textContent('#cfg-search_defaultPatternType option:first-child');
	if (!inherited.includes('regexp')) fail(`the inherited value is not shown: ${inherited}`);
	await admin.waitForSelector('#settings-panel :text("Inherited from the global settings.")');
	await admin.check('#cfg-orgs_allMembersBatchChangesAdmin');
	await admin.click('form.profile button:has-text("Save")');
	await admin.waitForSelector('.alert-success:has-text("Reload to see the new name")');
	if (!(await admin.isChecked('#cfg-orgs_allMembersBatchChangesAdmin'))) fail('saving the profile dropped unsaved settings');
	await admin.click('.settings-toolbar button:text-is("Save")');
	await admin.waitForSelector('.alert-success:text-is("Saved.")');
	await admin.screenshot({ path: `${OUT}/a06-organization-settings.png`, fullPage: true });
	await admin.reload();
	await admin.waitForSelector('#cfg-orgs_allMembersBatchChangesAdmin:checked');

	step('clear the global settings for the checks after this one');
	await admin.goto(`${BASE}/site-admin/global-settings`);
	await admin.click('button[role=tab]:has-text("Edit as JSON")');
	await admin.waitForSelector('.cm-content');
	await admin.click('.cm-content');
	await admin.keyboard.press('Control+A');
	await admin.keyboard.insertText('{}');
	await admin.click('.settings-toolbar button:text-is("Save")');
	await admin.waitForSelector('.alert-success:text-is("Saved.")');

	step('site configuration: lists of objects are forms too');
	await admin.goto(`${BASE}/site-admin/configuration?section=repositories`);
	await admin.click('button:has-text("Add git update interval")');
	await admin.fill('#cfg-gitUpdateInterval--0--pattern', '^gitlab');
	await admin.fill('#cfg-gitUpdateInterval--0--interval', '30');
	await admin.waitForTimeout(400);
	await admin.screenshot({ path: `${OUT}/a07-config-list.png`, fullPage: true });
	await admin.click('button[role=tab]:has-text("Edit as JSON")');
	const configJson = await admin.textContent('.cm-content');
	if (!/"interval":\s*30/.test(configJson)) fail(`the interval is not in the document: ${configJson}`);
	await admin.goto(`${BASE}/site-admin/users`);

	step('related links: users to the sign-in settings and back');
	await admin.click('nav[aria-label="Related pages"] a:has-text("Authentication & sign-in settings")');
	await admin.waitForURL(/\/site-admin\/configuration\?section=auth$/);
	await admin.waitForSelector('#settings-title:has-text("Authentication & sign-in")');
	await admin.click('.settings-head nav[aria-label="Related pages"] a:text-is("Users")');
	await admin.waitForURL(/\/site-admin\/users$/);
	await admin.goto(`${BASE}/site-admin/external-services/new`);
	await admin.waitForSelector('.page-description a:has-text("Authentication & sign-in")');

	step('find a setting by name');
	await admin.goto(`${BASE}/site-admin/configuration`);
	await admin.fill('#key-search', 'session expiry');
	await admin.click('nav[aria-label="Matching settings"] button:has-text("auth.sessionExpiry")');
	await admin.waitForSelector('#settings-title:has-text("Authentication & sign-in")');
	if ((await admin.inputValue('#key-search')) !== '') fail('the settings search was not cleared');

	step('setup checklist on the overview, and hiding it');
	await admin.goto(`${BASE}/site-admin`);
	await admin.waitForSelector('#setup-title:has-text("Set up Sourcegull")');
	await admin.waitForSelector('.checklist a:has-text("Add connection")');
	await admin.screenshot({ path: `${OUT}/a04-overview-setup.png` });
	await admin.click('.checklist button:has-text("Hide")');
	await admin.waitForSelector('#setup-title', { state: 'detached' });
	await admin.reload();
	await admin.waitForSelector('#health-title', { state: 'attached' });
	if (await admin.$('#setup-title')) fail('the hidden setup checklist came back');

	step('connect an agent: browser sign-in or an access token, not both');
	await admin.goto(`${BASE}/connect-agent`);
	await admin.waitForSelector('legend:has-text("How should the agent sign in?")');
	if (await admin.isVisible('#step-token')) fail('the token steps show with browser sign-in chosen');
	await admin.screenshot({ path: `${OUT}/a04-connect-agent-oauth.png`, fullPage: true });
	await admin.check('input[name="method"][value="token"]');
	await admin.waitForSelector('#step-token');
	if (await admin.isVisible('#step-oauth')) fail('the browser sign-in steps show with a token chosen');
	await admin.screenshot({ path: `${OUT}/a04-connect-agent-token.png`, fullPage: true });

	step('explain why the user can see the repository; sync now and the job history');
	await admin.goto(`${BASE}/site-admin/permissions`);
	await admin.fill('#explain-user', bob);
	await admin.fill('#explain-repo', REPO);
	await admin.click('button:has-text("Explain")');
	const verdict = (await admin.textContent('.explain [role=status] strong', { timeout: 10000 }))
		.replace(/\s+/g, ' ')
		.trim();
	console.log(`  ${verdict}`);
	if (!verdict.includes(`${bob} can read ${REPO}`)) fail(`unexpected explanation: ${verdict}`);
	// No connection here has an `authorization` block, so "sync now" explains that nothing syncs.
	await admin.click('.synced dd >> nth=0 >> button:has-text("Sync now")');
	const synced = await admin.textContent('.explain [role=alert]', { timeout: 10000 });
	if (!synced.includes('nothing to sync')) fail(`sync now: ${synced}`);
	await admin.selectOption('#job-state', 'FAILED');
	await admin.waitForSelector('td:has-text("No sync jobs.")');
	await admin.screenshot({ path: `${OUT}/a05-permissions.png`, fullPage: true });

	step('generate and delete an access token');
	await guest.goto(`${BASE}/user/settings/tokens`);
	await guest.click('button:has-text("Generate new token")');
	await guest.fill('#token-note', 'e2e');
	await guest.click('button:has-text("Generate token")');
	const token = await guest.inputValue('input[aria-label="New access token"]');
	if (!token.startsWith('skp_')) fail(`unexpected token format: ${token.slice(0, 4)}…`);
	const me = await guest.evaluate(
		(t) => fetch('/.api/graphql', { method: 'POST', headers: { authorization: `token ${t}` }, body: JSON.stringify({ query: '{ currentUser { username } }' }) }).then((r) => r.json()),
		token
	);
	if (me.data?.currentUser?.username !== bob) fail(`the token does not sign in: ${JSON.stringify(me)}`);
	await guest.screenshot({ path: `${OUT}/a06-tokens.png` });
	await guest.click('tr:has-text("e2e") button:has-text("Delete")');
	await guest.waitForSelector('td:has-text("You have no access tokens.")');

	step('delete the role');
	await admin.goto(`${BASE}/site-admin/roles`);
	await admin.click(`${card} button:has-text("Delete")`);
	await admin.waitForSelector(card, { state: 'detached' });

	step('mobile layout of the admin, tokens and contexts pages');
	await admin.setViewportSize({ width: 390, height: 844 });
	const pages = ['users', 'roles', 'permissions', 'access-requests', 'configuration'].map((p) => `/site-admin/${p}`);
	for (const path of ['/site-admin', ...pages, '/site-admin/global-settings', '/organizations/e2e-settings/settings', '/user/settings/preferences', '/user/settings/tokens', '/contexts', '/connect-agent']) {
		await admin.goto(`${BASE}${path}`);
		await admin.waitForSelector('h1');
		await admin.waitForTimeout(300);
		const overflow = await admin.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
		if (overflow > 0) {
			await admin.screenshot({ path: `${OUT}/a07-mobile${path.replaceAll('/', '-')}.png` });
			const wide = await admin.$$eval('*', (elements) =>
				elements
					.map((e) => ({ tag: e.tagName, className: String(e.className), right: Math.round(e.getBoundingClientRect().right) }))
					.filter((e) => e.right > window.innerWidth + 1)
					.sort((a, b) => b.right - a.right)
					.slice(0, 8)
			);
			console.log('  widest elements:', wide);
			fail(`${path} overflows by ${overflow}px at 390px`);
		}
	}
	await admin.goto(`${BASE}/site-admin/permissions`);
	await admin.waitForSelector('h1');
	await admin.screenshot({ path: `${OUT}/a07-mobile-permissions.png` });

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
