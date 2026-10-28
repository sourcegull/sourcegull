// Browser check of the Code trends creation wizards: from a dashboard,
// create a search based insight (validation, explicit repository list, live preview), see it
// on the dashboard, edit it; the capture group query checks; the group results and language
// usage previews; "Create trend" from search results; drafts restored and cleared on cancel.
// Fails on any page or console error. Screenshots go to OUT.
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
	const card = (title) => page.locator(`article[aria-label="${title} trend"]`);
	const preview = page.locator('[data-testid=live-preview]');
	const suffix = Date.now().toString(36);
	const title = `Wizard TODOs ${suffix}`;
	const edited = `Wizard TODOs edited ${suffix}`;

	step('sign in');
	await page.goto(`${BASE}/sign-in`);
	await page.fill('#username', 'admin');
	await page.fill('#password', 'correct horse battery');
	await page.click('button[type=submit]');
	await page.waitForURL((url) => !url.pathname.startsWith('/sign-in'));

	step('a dashboard to create from');
	const me = await graphql(`{ currentUser { id } }`);
	const created = await graphql(
		`mutation($input: CreateInsightsDashboardInput!) { createInsightsDashboard(input: $input) { dashboard { id } } }`,
		{ input: { title: `Wizards ${suffix}`, grants: { users: [me.currentUser.id] } } }
	);
	const dashboardId = created.createInsightsDashboard.dashboard.id;
	await page.goto(`${BASE}/trends/dashboards/${encodeURIComponent(dashboardId)}`);
	await page.click('a:has-text("Create trend")');
	await page.waitForURL(/\/trends\/create\?dashboardId=/);
	await page.waitForSelector('h1:has-text("Create new trend")');
	await page.screenshot({ path: `${OUT}/ic01-intro.png`, fullPage: true });

	step('search insight: validation');
	await page.click('a:has-text("Create track changes trend")');
	await page.waitForURL(/\/trends\/create\/search\?dashboardId=/);
	await page.waitForSelector('[data-testid=live-preview][data-status=intact]');
	await page.click('button[type=submit]:has-text("Create trend")');
	await page.waitForSelector('text=Search repositories query is a required field, please fill in the field.');
	await page.waitForSelector('text=Title is a required field.');
	await page.waitForSelector('text=Name is a required field for data series.');
	await page.fill('[id^=series-query-]', 'TODO repo:foo');
	await page.locator('[id^=series-query-]').blur();
	await page.waitForSelector('text=Do not include a `repo:` filter');

	step('search insight: repositories, series and live preview');
	await page.check('input[name=repoMode][value=urls-list]');
	await page.fill('#insight-repositories', REPO);
	await page.press('#insight-repositories', 'Enter');
	await page.waitForSelector(`[aria-label="Chosen repositories"] :text("${REPO}")`);
	await page.fill('[id^=series-name-]', 'TODO');
	await page.fill('[id^=series-query-]', 'TODO');
	await page.click('.colors button[aria-label="#2f9e44"]');
	await page.waitForSelector('[data-testid=live-preview][data-status=data]', { timeout: 120000 });
	await preview.locator('svg[aria-label="Line chart"]').waitFor();
	if ((await preview.locator('svg[aria-label="Line chart"] a.point').count()) === 0) fail('preview has no points');
	await preview.locator('[aria-label=Legend] :text("TODO")').waitFor();
	await page.click('button:has-text("+ Add another data series")');
	await page.locator('[id^=series-name-]').nth(1).fill('FIXME');
	await page.locator('[id^=series-query-]').nth(1).fill('FIXME');
	await preview.locator('[aria-label=Legend] :text("FIXME")').waitFor({ timeout: 120000 });
	await page.fill('#insight-title', title);
	await page.check('input[name=step][value=WEEK]');
	await page.waitForSelector('text=one datapoint every week');
	await page.screenshot({ path: `${OUT}/ic02-search-form.png`, fullPage: true });

	step('save and see it on the dashboard');
	await page.click('button[type=submit]:has-text("Create trend")');
	await page.waitForURL((url) => url.pathname === `/trends/dashboards/${encodeURIComponent(dashboardId)}` || url.pathname === `/trends/dashboards/${dashboardId}`);
	await card(title).waitFor();
	await card(title).locator('svg[aria-label="Line chart"]').waitFor({ timeout: 120000 });
	await page.screenshot({ path: `${OUT}/ic03-dashboard.png`, fullPage: true });
	const views = await graphql(
		`query($id: ID!) { insightsDashboards(id: $id) { nodes { views { nodes { id
			presentation { ... on LineChartInsightViewPresentation { title seriesPresentation { label color } } }
			dataSeriesDefinitions { ... on SearchInsightDataSeriesDefinition { query timeScope { ... on InsightIntervalTimeScope { unit value } }
				repositoryDefinition { ... on InsightRepositoryScope { repositories } } } } } } } } }`,
		{ id: dashboardId }
	);
	const view = views.insightsDashboards.nodes[0].views.nodes.find((v) => v.presentation.title === title);
	if (!view) fail('the new trend is not on the dashboard');
	const colors = view.presentation.seriesPresentation.map((s) => `${s.label}:${s.color}`).sort().join(',');
	if (!colors.startsWith('FIXME:') || !colors.includes('TODO:#2f9e44')) fail(`unexpected series ${colors}`);
	const d0 = view.dataSeriesDefinitions[0];
	if (d0.timeScope.unit !== 'WEEK' || d0.repositoryDefinition.repositories[0] !== REPO) fail('unexpected scope');

	step('edit the trend');
	await page.goto(`${BASE}/trends/${encodeURIComponent(view.id)}/edit?dashboardId=${encodeURIComponent(dashboardId)}`);
	await page.waitForSelector('h1:has-text("Edit trend")');
	if ((await page.inputValue('#insight-title')) !== title) fail('edit form did not load the title');
	if ((await page.locator('[id^=series-query-]').count()) !== 2) fail('edit form did not load both series');
	await page.waitForSelector('[data-testid=live-preview][data-status=data]', { timeout: 120000 });
	await page.fill('#insight-title', edited);
	await page.locator('.series-card').nth(1).locator('button:has-text("Remove series")').click();
	await page.click('button[type=submit]:has-text("Save changes")');
	await page.waitForURL(/\/trends\/dashboards\//);
	await card(edited).waitFor();
	await card(edited).locator('[aria-label=Legend] :text("FIXME")').waitFor({ state: 'detached' });

	step('capture group insight: query checks and preview');
	await page.goto(`${BASE}/trends/create/capture-group`);
	await page.fill('#capture-group-query', 'TODO repo:x');
	await page.locator('#capture-group-query').blur();
	await page.waitForSelector('text=Query is not valid');
	await page.waitForSelector('[aria-label="Search query validation checks list"] li.invalid:has-text("Does not contain repo: filter")');
	await page.waitForSelector('[aria-label="Search query validation checks list"] li.invalid:has-text("capture group")');
	await page.fill('#capture-group-query', 'file:\\.rs$ (TODO|FIXME)');
	await page.waitForSelector('text=Query is not valid', { state: 'detached' });
	await page.check('input[name=repoMode][value=all-repos]');
	await page.waitForSelector('[data-testid=live-preview][data-status=data]', { timeout: 120000 });
	await preview.locator('svg[aria-label="Line chart"]').waitFor();
	await page.screenshot({ path: `${OUT}/ic04-capture-group.png`, fullPage: true });

	step('group results preview');
	await page.goto(`${BASE}/trends/create/group-results`);
	await page.fill('#insight-repositories', REPO);
	await page.press('#insight-repositories', 'Enter');
	await page.fill('[id^=series-name-]', 'TODO');
	await page.fill('[id^=series-query-]', 'TODO');
	if (!(await page.locator('[role=radio]:has-text("author")').isDisabled())) fail('author needs a commit search');
	await page.click('[role=radio]:has-text("language")');
	await page.waitForSelector('[data-testid=live-preview][data-status=data]', { timeout: 120000 });
	await preview.locator('svg[aria-label="Bar chart"]').waitFor();
	await page.screenshot({ path: `${OUT}/ic05-group-results.png`, fullPage: true });

	step('language usage preview');
	await page.goto(`${BASE}/trends/create/lang-stats`);
	await page.fill('#insight-repository', 'gitlab.example.com/acme/missing');
	await page.locator('#insight-repository').blur();
	await page.waitForSelector("text=We couldn't find the repository gitlab.example.com/acme/missing.");
	await page.fill('#insight-repository', REPO);
	await page.waitForSelector('[data-testid=live-preview][data-status=data]', { timeout: 120000 });
	await preview.locator('svg[aria-label="Pie chart"]').waitFor();
	await page.screenshot({ path: `${OUT}/ic06-lang-stats.png`, fullPage: true });

	step('create trend from search results');
	await page.goto(`${BASE}/search?q=${encodeURIComponent(`repo:^${REPO.replace(/\./g, '\\.')}$ TODO`)}&patternType=keyword`);
	await page.click('a:has-text("Create trend")');
	await page.waitForURL(/\/trends\/create\/search\?query=/);
	await page.waitForSelector('h1:has-text("Create trend")');
	const repoQuery = await page.inputValue('#insight-repo-query');
	if (!repoQuery.startsWith('repo:^')) fail(`unexpected repository query ${repoQuery}`);
	const seriesQuery = await page.inputValue('[id^=series-query-]');
	if (seriesQuery !== 'TODO patterntype:keyword') fail(`unexpected series query ${seriesQuery}`);
	if ((await page.inputValue('[id^=series-name-]')) !== 'Search series #1') fail('unexpected series name');
	await page.screenshot({ path: `${OUT}/ic06b-from-search.png`, fullPage: true });

	step('drafts: restored on return, cleared on cancel');
	await page.goto(`${BASE}/trends/create/lang-stats`);
	await page.fill('#insight-title', `Draft ${suffix}`);
	await page.goto(`${BASE}/trends/create/lang-stats`);
	await page.waitForSelector('#insight-title');
	if ((await page.inputValue('#insight-title')) !== `Draft ${suffix}`) fail('the draft was not restored');
	await page.click('button:has-text("Cancel")');
	await page.waitForURL((url) => !url.pathname.startsWith('/trends/create'));
	await page.goto(`${BASE}/trends/create/lang-stats`);
	await page.waitForSelector('#insight-title');
	if ((await page.inputValue('#insight-title')) !== '') fail('cancel did not clear the draft');

	step('phone layout');
	await page.setViewportSize({ width: 390, height: 844 });
	await page.goto(`${BASE}/trends/create/search`);
	await page.waitForSelector('#insight-title');
	const overflow = await page.evaluate(() => document.documentElement.scrollWidth > window.innerWidth + 1);
	if (overflow) fail('the creation page scrolls horizontally on a phone');
	await page.screenshot({ path: `${OUT}/ic07-mobile.png`, fullPage: true });

	step('clean up');
	await graphql(`mutation($id: ID!) { deleteInsightView(id: $id) { alwaysNil } }`, { id: view.id });
	await graphql(`mutation($id: ID!) { deleteInsightsDashboard(id: $id) { alwaysNil } }`, { id: dashboardId });

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
