// Browser check of Gitea sign-in and repository permissions against a real Gitea
// (compose service `gitea`, started empty by `scripts/dev gitea up` with the site administrator
// root). Seeds Gitea through its API: users gitea-ann and gitea-ben, a private repository
// root/secret that only gitea-ann can read (as a collaborator), an OAuth2 application. Then
// configures Sourcegull with a Gitea connection whose `authorization` uses the OAuth identity
// provider and a `gitea` sign-in provider, signs in as each user through Gitea in the browser and
// checks: gitea-ann finds the repository in search, opens a file and reads it through MCP with an
// access token; gitea-ben sees none of it. After gitea-ann's collaborator access is removed on
// Gitea and a permissions sync is scheduled, her access is gone too. Fails on any page error.
// Screenshots go to OUT.
//
// Run it with `CHECKS=gitea scripts/dev e2e` (signs up the admin when no check did before it).
const { chromium } = require('playwright');
const BASE = process.env.BASE || 'http://127.0.0.1:7090';
const OUT = process.env.OUT || require('node:os').tmpdir();
const GITEA = (process.env.GITEA_URL || 'http://gitea:3000').replace(/\/$/, '');
const GITEA_ROOT = { username: 'root', password: process.env.GITEA_ROOT_PASSWORD || 'Jk5r-Tw2q-Vb7n' };
const HEADERS = { 'X-Requested-With': 'Sourcegull' };
const PASSWORD = 'Pq8w-Gitea-Ux3e';
const MARKER = 'giteaSecretMarker';
const FILE = 'secret.go';

function step(name) {
	console.log(`- ${name}`);
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

(async () => {
	const browser = await chromium.launch({ executablePath: process.env.CHROMIUM_EXECUTABLE });
	const problems = [];
	const fail = (msg) => {
		throw new Error(msg);
	};
	const newPage = async () => {
		const context = await browser.newContext({ viewport: { width: 1400, height: 900 }, bypassCSP: true });
		const page = await context.newPage();
		page.on('pageerror', (e) => problems.push(`pageerror: ${e.message}`));
		page.on('console', (m) => {
			// Gitea's pages and Sourcegull's 404 for an unreadable file log failed requests.
			if (m.type() === 'error' && !m.text().includes('status of 4') && !page.url().startsWith(GITEA)) {
				problems.push(`console: ${m.text()}`);
			}
		});
		return page;
	};
	const until = async (what, fn, timeout = 120000) => {
		const end = Date.now() + timeout;
		for (;;) {
			const v = await fn();
			if (v) return v;
			if (Date.now() > end) fail(`timed out waiting for ${what}`);
			await sleep(1000);
		}
	};

	// Gitea's API as root, with basic auth.
	const auth = 'Basic ' + Buffer.from(`${GITEA_ROOT.username}:${GITEA_ROOT.password}`).toString('base64');
	const gitea = async (method, path, body) => {
		const r = await fetch(`${GITEA}/api/v1${path}`, {
			method,
			headers: { Authorization: auth, 'Content-Type': 'application/json' },
			body: body === undefined ? undefined : JSON.stringify(body)
		});
		const text = await r.text();
		if (!r.ok) fail(`gitea ${method} ${path}: ${r.status} ${text}`);
		return text ? JSON.parse(text) : null;
	};
	// Sourcegull's GraphQL API with a page's session.
	const graphql = async (page, query, variables = {}) => {
		const r = await page.request.post(`${BASE}/.api/graphql`, { headers: HEADERS, data: { query, variables } });
		const body = await r.json();
		if (body.errors) fail(`graphql: ${JSON.stringify(body.errors)}`);
		return body.data;
	};

	step('seed Gitea: users, a private repository with a collaborator, an OAuth2 application');
	for (const name of ['gitea-ann', 'gitea-ben']) {
		await gitea('POST', '/admin/users', {
			username: name,
			email: `${name}@example.com`,
			password: PASSWORD,
			must_change_password: false
		});
	}
	await gitea('POST', '/user/repos', { name: 'secret', private: true, auto_init: true, default_branch: 'main' });
	await gitea('POST', `/repos/root/secret/contents/${FILE}`, {
		message: 'Add the secret',
		content: Buffer.from(`package secret\n\n// ${MARKER} is readable only by collaborators.\nconst ${MARKER} = 42\n`).toString(
			'base64'
		)
	});
	await gitea('PUT', '/repos/root/secret/collaborators/gitea-ann', { permission: 'read' });
	const token = await gitea('POST', '/users/root/tokens', {
		name: `sourcegull-e2e-${Date.now()}`,
		scopes: ['read:repository', 'read:organization', 'read:user']
	});
	const app = await gitea('POST', '/user/applications/oauth2', {
		name: 'sourcegull-e2e',
		redirect_uris: [`${BASE}/.auth/gitea/callback`],
		confidential_client: true
	});

	step('sign in to Sourcegull as the site admin');
	const admin = await newPage();
	const credentials = { username: 'admin', password: 'correct horse battery' };
	const signIn = await admin.request.post(`${BASE}/-/sign-in`, { headers: HEADERS, data: credentials });
	if (!signIn.ok()) {
		// No check signed up the admin before this one.
		const signUp = await admin.request.post(`${BASE}/-/sign-up`, {
			headers: HEADERS,
			data: { ...credentials, email: 'admin@example.com' }
		});
		if (!signUp.ok()) fail(`sign-up: ${signUp.status()} ${await signUp.text()}`);
	}

	step('configure the Gitea sign-in provider and connection');
	const { site } = await graphql(admin, '{ site { configuration { id effectiveContents } } }');
	const provider = JSON.stringify({
		type: 'gitea',
		url: `${GITEA}/`,
		clientID: app.client_id,
		clientSecret: app.client_secret,
		allowSignup: true,
		displayName: 'Gitea'
	});
	let contents = site.configuration.effectiveContents;
	if (/^\s*"auth\.providers"\s*:\s*\[/m.test(contents)) {
		contents = contents.replace(/("auth\.providers"\s*:\s*\[)/, `$1\n    ${provider},`);
	} else {
		contents = contents.replace('{', `{\n  "auth.providers": [{ "type": "builtin", "allowSignup": true }, ${provider}],`);
	}
	if (!/^\s*"externalURL"/m.test(contents)) contents = contents.replace('{', `{\n  "externalURL": "${BASE}",`);
	await graphql(admin, 'mutation($id: Int!, $input: String!) { updateSiteConfiguration(lastID: $id, input: $input) }', {
		id: site.configuration.id,
		input: contents
	});
	const config = JSON.stringify({
		url: GITEA,
		token: token.sha1,
		repos: ['root/secret'],
		repositoryQuery: ['none'],
		authorization: { identityProvider: { type: 'oauth' } }
	});
	const { addExternalService: conn } = await graphql(
		admin,
		'mutation($input: AddExternalServiceInput!) { addExternalService(input: $input) { id } }',
		{ input: { kind: 'GITEA', displayName: 'Gitea (e2e)', config } }
	);
	await graphql(admin, 'mutation($id: ID!) { syncExternalService(id: $id) { alwaysNil } }', { id: conn.id });

	step('the repository is mirrored and searchable by the site admin');
	const repo = await until('the Gitea repository', async () => {
		const d = await graphql(admin, '{ repositories(first: 50, query: "root/secret") { nodes { name mirrorInfo { cloned } } } }');
		const r = d.repositories.nodes.find((n) => n.name.endsWith('/root/secret'));
		return r && r.mirrorInfo.cloned ? r.name : null;
	});
	console.log(`  repository: ${repo}`);
	// Streamed search as a page's user; the number of file matches in the repository.
	const matches = async (page) => {
		const q = encodeURIComponent(`repo:^${repo.replace(/[.]/g, '\\.')}$ ${MARKER}`);
		const r = await page.request.get(`${BASE}/.api/search/stream?q=${q}&v=V3&t=keyword`, { headers: HEADERS });
		const text = await r.text();
		return (text.match(new RegExp(`"path":"${FILE}"`, 'g')) || []).length;
	};
	await until('search results for the site admin', async () => (await matches(admin)) > 0);

	const signInWithGitea = async (name) => {
		const page = await newPage();
		await page.goto(`${BASE}/sign-in`);
		await page.click('a:has-text("Sign in with Gitea")');
		await page.waitForURL((u) => u.href.startsWith(GITEA));
		await page.fill('#user_name', name);
		await page.fill('#password', PASSWORD);
		await page.click('form[action="/user/login"] button.primary');
		await page.waitForURL((u) => !u.pathname.startsWith('/user/login'));
		if (page.url().startsWith(GITEA)) {
			// The grant page, the first time an application asks.
			await page.screenshot({ path: `${OUT}/g01-${name}-grant.png` });
			await page.click('#authorize-app');
		}
		await page.waitForURL((u) => u.href.startsWith(BASE) && !u.pathname.startsWith('/sign-in'), { timeout: 30000 });
		return page;
	};
	const blobURL = `${BASE}/${repo}/-/blob/${FILE}`;
	const canOpen = async (page) => {
		await page.goto(blobURL);
		await page.waitForLoadState('networkidle');
		return (await page.locator(`text=${MARKER}`).count()) > 0;
	};
	// MCP with the access token only, as an agent calls it (no session cookie).
	const mcpRead = async (accessToken) => {
		const r = await fetch(`${BASE}/.api/mcp`, {
			method: 'POST',
			headers: {
				Authorization: `Bearer ${accessToken}`,
				'Content-Type': 'application/json',
				Accept: 'application/json, text/event-stream'
			},
			body: JSON.stringify({
				jsonrpc: '2.0',
				id: 1,
				method: 'tools/call',
				params: { name: 'read_file', arguments: { repo, path: FILE } }
			})
		});
		const text = await r.text();
		if (r.status !== 200) fail(`mcp: ${r.status} ${text}`);
		return text.includes(MARKER);
	};
	const accessToken = async (page) => {
		const { currentUser } = await graphql(page, '{ currentUser { id username } }');
		const { createAccessToken } = await graphql(
			page,
			'mutation($user: ID!) { createAccessToken(user: $user, scopes: ["user:all"], note: "e2e mcp") { token } }',
			{ user: currentUser.id }
		);
		return { id: currentUser.id, username: currentUser.username, token: createAccessToken.token };
	};

	step('gitea-ann signs in through Gitea, finds and opens the private file, reads it through MCP');
	const ann = await signInWithGitea('gitea-ann');
	const annUser = await accessToken(ann);
	console.log(`  signed in as ${annUser.username}`);
	await until('gitea-ann to find the file', async () => (await matches(ann)) > 0);
	await ann.goto(`${BASE}/search?q=${encodeURIComponent(`repo:secret ${MARKER}`)}`);
	await ann.waitForSelector('.result', { timeout: 30000 });
	await ann.screenshot({ path: `${OUT}/g02-ann-search.png` });
	if (!(await canOpen(ann))) fail('gitea-ann cannot open the file');
	await ann.screenshot({ path: `${OUT}/g03-ann-file.png` });
	if (!(await mcpRead(annUser.token))) fail('gitea-ann cannot read the file through MCP');

	step('gitea-ben signs in through Gitea and sees none of it');
	const ben = await signInWithGitea('gitea-ben');
	const benUser = await accessToken(ben);
	// Wait for gitea-ben's sign-in sync to finish, so that a denial is not just a pending sync.
	await until('gitea-ben\'s permissions sync', async () => {
		const d = await graphql(
			admin,
			'query($id: ID!) { node(id: $id) { ... on User { permissionsInfo { syncedAt } } } }',
			{ id: benUser.id }
		);
		return d.node.permissionsInfo && d.node.permissionsInfo.syncedAt;
	});
	if ((await matches(ben)) !== 0) fail('gitea-ben finds the private file');
	if (await canOpen(ben)) fail('gitea-ben can open the private file');
	await ben.screenshot({ path: `${OUT}/g04-ben-file.png` });
	if (await mcpRead(benUser.token)) fail('gitea-ben reads the private file through MCP');

	step('remove gitea-ann\'s access on Gitea and sync her permissions');
	await gitea('DELETE', '/repos/root/secret/collaborators/gitea-ann');
	await graphql(admin, 'mutation($user: ID!) { scheduleUserPermissionsSync(user: $user) { alwaysNil } }', {
		user: annUser.id
	});
	await until('gitea-ann to lose access', async () => (await matches(ann)) === 0, 60000);
	if (await canOpen(ann)) fail('gitea-ann can still open the file');
	if (await mcpRead(annUser.token)) fail('gitea-ann still reads the file through MCP');
	await ann.screenshot({ path: `${OUT}/g05-ann-revoked.png` });

	if (problems.length) fail(`page problems:\n${problems.join('\n')}`);
	console.log('gitea: ok');
	await browser.close();
})().catch((e) => {
	console.error(e);
	process.exit(1);
});
