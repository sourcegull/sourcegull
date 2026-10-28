// Browser check of the Code trends dashboards: create a dashboard,
// add and remove views, the charts (line chart points, tooltip, rev:at.time() drill-down, legend,
// "Start Y Axis at 0", CSV export, the language pie), drill-down filters saved as defaults and
// as a new view, configure the dashboard's visibility, the All trends tab, deleting, and the
// phone layout. Fails on any
// page or console error. Screenshots go to OUT.
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
	const cardCount = () => page.locator('article.insight-card').count();
	const suffix = Date.now().toString(36);
	const lineTitle = `TODO markers ${suffix}`;
	const pieTitle = `Languages ${suffix}`;
	const newViewTitle = `TODO by name ${suffix}`;
	const dashboardTitle = `E2E dashboard ${suffix}`;

	step('sign in');
	await page.goto(`${BASE}/sign-in`);
	await page.fill('#username', 'admin');
	await page.fill('#password', 'correct horse battery');
	await page.click('button[type=submit]');
	await page.waitForURL((url) => !url.pathname.startsWith('/sign-in'));

	step('create trends through the API');
	const line = await graphql(
		`mutation($input: LineChartSearchInsightInput!) { createLineChartSearchInsight(input: $input) { view { id } } }`,
		{
			input: {
				dataSeries: [
					{ query: 'TODO', options: { label: 'TODO', lineColor: '#e8590c' } },
					{ query: 'FIXME', options: { label: 'FIXME', lineColor: '#1c7ed6' } }
				],
				repositoryScope: { repositories: [REPO] },
				timeScope: { stepInterval: { unit: 'MONTH', value: 1 } },
				options: { title: lineTitle }
			}
		}
	);
	const lineId = line.createLineChartSearchInsight.view.id;
	await graphql(
		`mutation($input: PieChartSearchInsightInput!) { createPieChartSearchInsight(input: $input) { view { id } } }`,
		{
			input: {
				query: '',
				repositoryScope: { repositories: [REPO] },
				presentationOptions: { title: pieTitle, otherThreshold: 0.03 }
			}
		}
	);

	step('nav entry and landing');
	await page.goto(`${BASE}/search`);
	await page.click('nav[aria-label=Main] a:has-text("Trends")');
	await page.waitForURL(/\/trends\/(dashboards|about)/);
	await page.waitForSelector('h1:has-text("Trends")');

	step('add a private dashboard');
	await page.click('a:has-text("Add dashboard")');
	await page.waitForURL(/\/trends\/add-dashboard$/);
	await page.click('button[type=submit]:has-text("Add dashboard")');
	await page.waitForSelector('text=Name is a required field.');
	await page.fill('#dashboard-name', dashboardTitle);
	await page.click('button[type=submit]:has-text("Add dashboard")');
	await page.waitForURL(/\/trends\/dashboards\/[^/]+$/);
	const dashboardURL = page.url();
	await page.waitForSelector(`button[aria-label="Choose a dashboard, ${dashboardTitle}"]:has-text("Private")`);
	await page.waitForSelector('[data-testid=add-insights-button-card]');
	await page.screenshot({ path: `${OUT}/in01-empty-dashboard.png`, fullPage: true });

	step('add trends to the dashboard');
	await page.click('[data-testid=add-insights-button-card]');
	await page.waitForSelector('dialog[open] h2:has-text("Add trend to")');
	await page.fill('#insight-find', suffix);
	await page.waitForSelector(`dialog label:has-text("${lineTitle}")`);
	await page.check(`dialog label:has-text("${lineTitle}") input`);
	await page.check(`dialog label:has-text("${pieTitle}") input`);
	await page.click('dialog button[type=submit]:has-text("Save")');
	await page.waitForSelector('dialog[open]', { state: 'detached' });
	await card(lineTitle).waitFor();
	await card(pieTitle).waitFor();
	await card(lineTitle).locator('[data-testid=insight-card-body], .loading').first().waitFor();
	await page.screenshot({ path: `${OUT}/in02-dashboard.png`, fullPage: true });

	step('line chart: points after the backfill');
	const lineCard = card(lineTitle);
	await lineCard.locator('svg[aria-label="Line chart"]').waitFor();
	// The card polls while the series backfill; points appear once the first ones are recorded.
	const points = lineCard.locator('svg[aria-label="Line chart"] a.point');
	await points.first().waitFor({ timeout: 120000 });
	await lineCard.locator('text=This trend is still being processed').waitFor({ state: 'detached', timeout: 120000 });
	if ((await lineCard.locator('[aria-label=Legend] [role=checkbox]').count()) !== 2) fail('legend should list 2 series');

	step('tooltip on hover');
	const lastPoint = lineCard.locator('svg [aria-label=TODO] a.point').last();
	await lastPoint.hover();
	const tooltip = lineCard.locator('[role=tooltip]');
	await tooltip.waitFor();
	const tipText = await tooltip.innerText();
	if (!/\d{4}/.test(tipText) || !tipText.includes('TODO') || !tipText.includes('FIXME')) fail(`tooltip: ${tipText}`);
	await page.screenshot({ path: `${OUT}/in02b-line-tooltip.png` });

	step('drill-down to the point-in-time search');
	const href = await lastPoint.getAttribute('href');
	const drill = new URL(href, BASE);
	const q = drill.searchParams.get('q') || '';
	if (drill.pathname !== '/search' || !q.includes('rev:at.time(') || !q.includes('TODO')) {
		fail(`drill-down link: ${href}`);
	}
	const [popup] = await Promise.all([context.waitForEvent('page'), lastPoint.click()]);
	await popup.waitForLoadState();
	if (!decodeURIComponent(popup.url().replace(/\+/g, ' ')).includes('rev:at.time(')) fail(`popup: ${popup.url()}`);
	await popup.close();

	step('legend toggles series');
	await lineCard.locator('[aria-label=Legend] [role=checkbox]:has-text("FIXME")').click();
	await lineCard.locator('svg [aria-label=TODO]').waitFor({ state: 'detached' });
	await lineCard.locator('[aria-label=Legend] [role=checkbox]:has-text("FIXME")').click();
	await lineCard.locator('svg [aria-label=TODO]').waitFor();

	step('start Y axis at 0');
	await lineCard.locator('button[aria-label="Trend options"]').click();
	const zero = page.locator('[role=menuitemcheckbox]:has-text("Start Y Axis at 0")');
	if ((await zero.getAttribute('aria-checked')) !== 'false') fail('Y axis option on by default');
	await zero.click();
	if ((await zero.getAttribute('aria-checked')) !== 'true') fail('Y axis option not toggled');
	await page.keyboard.press('Escape');
	const lowest = await lineCard.locator('svg[aria-label="Line chart"] text.tick').first().textContent();
	if (lowest !== '0') fail(`lowest Y tick should be 0, got ${lowest}`);

	step('export data as CSV');
	await lineCard.locator('button[aria-label="Trend options"]').click();
	await page.click(`article[aria-label="${lineTitle} trend"] [role=menuitem]:has-text("Export data")`);
	await page.waitForSelector('dialog[open] h2:has-text("Export data for")');
	const exportLink = page.locator('dialog[open] a:has-text("Export data as CSV")');
	const exportHref = await exportLink.getAttribute('href');
	if (exportHref !== `/.api/insights/export/${encodeURIComponent(lineId)}`) fail(`export link: ${exportHref}`);
	const [download] = await Promise.all([page.waitForEvent('download'), exportLink.click()]);
	if (!download.suggestedFilename().endsWith('.zip')) fail(`download name: ${download.suggestedFilename()}`);
	await page.waitForSelector('dialog[open]', { state: 'detached' });
	const zip = await page.request.get(`${BASE}${exportHref}`);
	if (zip.status() !== 200 || zip.headers()['content-type'] !== 'application/zip') fail(`export: ${zip.status()}`);

	step('language pie');
	const pieCard = card(pieTitle);
	const slices = pieCard.locator('svg[aria-label="Pie chart"] a[data-language]');
	await slices.first().waitFor({ timeout: 30000 });
	const sliceHref = await slices.first().getAttribute('href');
	if (!/[?&]q=repo%3A/.test(sliceHref) || !sliceHref.includes('lang%3A')) fail(`slice link: ${sliceHref}`);
	await slices.first().hover();
	await pieCard.locator('button[aria-label="Trend options"]').click();
	if ((await page.locator(`article[aria-label="${pieTitle} trend"] [role=menuitem]:has-text("Export data")`).count()) !== 0) {
		fail('pie charts have no data export');
	}
	await page.keyboard.press('Escape');
	await page.screenshot({ path: `${OUT}/in02c-charts.png`, fullPage: true });
	await page.emulateMedia({ colorScheme: 'dark' });
	await page.screenshot({ path: `${OUT}/in02d-charts-dark.png`, fullPage: true });
	await page.emulateMedia({ colorScheme: 'light' });

	step('drill-down filters saved as defaults');
	await card(lineTitle).locator('button[aria-label=Filters]').click();
	await page.waitForSelector('form[aria-label="Drill-down filters panel"]');
	await page.fill('#filter-include', '(');
	await page.waitForSelector('text=Must be a valid regular expression.');
	if (!(await page.locator('button:has-text("Save default filters")').isDisabled())) fail('saving an invalid regexp');
	await page.fill('#filter-include', 'acme/');
	await page.click('button:has-text("Save default filters")');
	await page.waitForSelector('form[aria-label="Drill-down filters panel"]', { state: 'detached' });
	await card(lineTitle).locator('button[aria-label="Active filters"]').waitFor();
	const saved = await graphql(`query($id: ID!) { insightViews(id: $id) { nodes { defaultFilters { includeRepoRegex } } } }`, {
		id: lineId
	});
	if (saved.insightViews.nodes[0].defaultFilters.includeRepoRegex !== 'acme/') fail('default filters not saved');

	step('save as new view');
	await card(lineTitle).locator('button[aria-label="Active filters"]').click();
	await page.waitForSelector('text=Default filters applied');
	await page.click('summary:has-text("Sort & Limit")');
	await page.click('button[aria-label="Sort by name with ascending order"]');
	await page.fill('input[aria-label="Number of data series"]', '1');
	await page.screenshot({ path: `${OUT}/in03-filters.png`, fullPage: true });
	await page.click('button:has-text("Save as new view")');
	await page.click('form[aria-label="Save as new view"] button[type=submit]');
	await page.waitForSelector('text=Trend name is a required field.');
	await page.fill('#new-view-name', newViewTitle);
	await page.click('form[aria-label="Save as new view"] button[type=submit]');
	await card(newViewTitle).waitFor({ timeout: 15000 });
	if ((await cardCount()) !== 3) fail(`expected 3 cards, got ${await cardCount()}`);

	step('remove a view from the dashboard');
	await card(newViewTitle).locator('button[aria-label="Trend options"]').click();
	await page.click('[role=menuitem]:has-text("Remove from this dashboard")');
	await card(newViewTitle).waitFor({ state: 'detached' });
	if ((await cardCount()) !== 2) fail('view not removed');

	step('configure the dashboard');
	await page.click('button[aria-label="Dashboard options"]');
	await page.click('[role=menuitem]:has-text("Configure dashboard")');
	await page.waitForURL(/\/edit$/);
	await page.waitForSelector('h1:has-text("Configure dashboard")');
	if ((await page.inputValue('#dashboard-name')) !== dashboardTitle) fail('edit form not filled in');
	await page.fill('#dashboard-name', `${dashboardTitle} renamed`);
	await page.check('input[name=visibility][value=global]');
	await page.click('button[type=submit]:has-text("Save changes")');
	await page.waitForURL(dashboardURL);
	await page.waitForSelector(`button[aria-label="Choose a dashboard, ${dashboardTitle} renamed"]:has-text("Global")`);
	await page.click('button[aria-label^="Choose a dashboard"]');
	await page.waitForSelector(`[role=group][aria-label=Global] a:has-text("${dashboardTitle} renamed")`);
	await page.keyboard.press('Escape');

	step('all trends');
	await page.click('nav[aria-label=Trends] a:has-text("All trends")');
	await page.waitForURL(/\/trends\/all\?dashboardId=/);
	await card(newViewTitle).waitFor();
	await card(lineTitle).waitFor();
	await card(lineTitle).locator('button[aria-label="Trend options"]').click();
	if (!(await page.locator('[role=menuitem]:has-text("Remove from this dashboard")').isDisabled())) {
		fail('remove should be disabled on All trends');
	}
	await page.keyboard.press('Escape');

	step('delete an trend');
	await card(newViewTitle).locator('button[aria-label="Trend options"]').click();
	await page.click(`article[aria-label="${newViewTitle} trend"] [role=menuitem]:has-text("Delete")`);
	await card(newViewTitle).waitFor({ state: 'detached' });

	step('standalone trend page');
	await card(lineTitle).locator('h3 a').click();
	await page.waitForURL(/\/trends\/[^/]+$/);
	await card(lineTitle).waitFor();

	step('phone layout');
	await page.setViewportSize({ width: 390, height: 844 });
	for (const path of [new URL(dashboardURL).pathname, '/trends/all', '/trends/add-dashboard', '/trends/about']) {
		await page.goto(`${BASE}${path}`);
		await page.waitForTimeout(800);
		const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
		if (overflow > 0) fail(`${path} overflows by ${overflow}px at 390px`);
	}
	await page.goto(dashboardURL);
	await card(lineTitle).waitFor();
	await page.screenshot({ path: `${OUT}/in04-mobile.png`, fullPage: true });
	await page.setViewportSize({ width: 1400, height: 900 });

	step('delete the dashboard');
	await page.click('button[aria-label="Dashboard options"]');
	await page.click('[role=menuitem]:has-text("Delete")');
	await page.waitForURL((url) => url.href !== dashboardURL);
	const left = await graphql(`{ insightsDashboards { nodes { title } } }`);
	if (left.insightsDashboards.nodes.some((d) => d.title.startsWith(dashboardTitle))) fail('dashboard not deleted');

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
