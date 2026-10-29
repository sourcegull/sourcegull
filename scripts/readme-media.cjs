// The README media: screenshots of code navigation in the light and dark themes, and frames
// of a search being typed (frames/, for the animated WebP).
// Run it with scripts/readme-media.sh, which sets up the server and the repositories.
const fs = require('node:fs');
const { chromium } = require('playwright');
const BASE = process.env.BASE || 'http://127.0.0.1:7090';
const OUT = process.env.OUT || require('node:os').tmpdir();
const VIEWPORT = { width: 1280, height: 760 };
const QUERY = process.env.QUERY || 'lang:rust serde::Deserialize';
const REPO = 'github.com/tokio-rs/tokio';
const PATH = 'tokio/src/fs/read.rs';
// The hovered identifier is the first word of the first line that contains NAV_LINE.
const NAV_LINE = process.env.NAV_LINE || 'asyncify(move ||';

function step(name) {
	console.log(`- ${name}`);
}

async function signIn(page) {
	await page.goto(`${BASE}/sign-in`);
	await page.fill('#username', 'admin');
	await page.fill('#password', 'correct horse battery');
	await page.click('button[type=submit]');
	await page.waitForURL((url) => !url.pathname.startsWith('/sign-in'));
}

async function results(page) {
	await page.waitForSelector('.result', { timeout: 30000 });
	await page.waitForFunction(() => !document.querySelector('.spinner'), null, { timeout: 30000 });
	await page.mouse.move(0, VIEWPORT.height - 1);
	await page.waitForTimeout(500);
}

// The screen rectangle of the first `word` on the code line containing `lineText`.
const wordBox = (page, lineText, word) =>
	page.evaluate(
		([lineText, word]) => {
			const line = [...document.querySelectorAll('.cm-line')].find((l) => l.textContent.includes(lineText));
			if (!line) return null;
			const walker = document.createTreeWalker(line, NodeFilter.SHOW_TEXT);
			const start = line.textContent.indexOf(lineText) + lineText.indexOf(word);
			let seen = 0;
			for (let n = walker.nextNode(); n; n = walker.nextNode()) {
				const len = n.textContent.length;
				if (start < seen + len) {
					const r = document.createRange();
					r.setStart(n, start - seen);
					r.setEnd(n, Math.min(start - seen + word.length, len));
					const b = r.getBoundingClientRect();
					return { x: b.left + b.width / 2, y: b.top + b.height / 2 };
				}
				seen += len;
			}
			return null;
		},
		[lineText, word]
	);

async function shots(browser, theme) {
	const context = await browser.newContext({ viewport: VIEWPORT, deviceScaleFactor: 2, colorScheme: theme });
	const page = await context.newPage();
	await signIn(page);

	step(`${theme}: code navigation`);
	const raw = await (await page.request.get(`${BASE}/${REPO}/-/raw/${PATH}`)).text();
	const lineNo = raw.split('\n').findIndex((l) => l.includes(NAV_LINE)) + 1;
	if (lineNo === 0) throw new Error(`no ${NAV_LINE} in ${PATH}`);
	await page.goto(`${BASE}/${REPO}/-/blob/${PATH}#L${lineNo}`);
	await page.waitForSelector(`.cm-selected-line:has-text(${JSON.stringify(NAV_LINE)})`, { timeout: 30000 });
	const toggle = await page.$('button[aria-label="Hide file tree"]');
	if (toggle) await toggle.click();
	// Centred, so the hover card has room above the line.
	await page.$eval('.cm-selected-line', (l) => l.scrollIntoView({ block: 'center' }));
	await page.waitForTimeout(500);
	const box = await wordBox(page, NAV_LINE, NAV_LINE.replace(/\W.*$/, ''));
	await page.mouse.move(box.x, box.y);
	await page.waitForSelector('.cm-codenav-hover', { timeout: 15000 });
	await page.click('.cm-codenav-hover button:has-text("Find references")');
	await page.waitForSelector('.usages .line', { timeout: 30000 });
	// The panel covers the lower half of the code: bring the line back and hover it again.
	await page.mouse.move(5, 5);
	await page.$eval('.cm-selected-line', (l) => l.scrollIntoView({ block: 'center' }));
	await page.waitForTimeout(500);
	const moved = await wordBox(page, NAV_LINE, NAV_LINE.replace(/\W.*$/, ''));
	await page.mouse.move(moved.x, moved.y);
	await page.waitForSelector('.cm-codenav-hover', { timeout: 15000 });
	await page.waitForTimeout(500);
	await page.screenshot({ path: `${OUT}/readme-codenav-${theme}.png` });
	await context.close();
}

(async () => {
	const browser = await chromium.launch({ executablePath: process.env.CHROMIUM_EXECUTABLE });

	step('sign up the first user (once per instance)');
	{
		const context = await browser.newContext({ viewport: VIEWPORT });
		const page = await context.newPage();
		await page.goto(`${BASE}/sign-up`);
		const email = await page.waitForSelector('#email', { timeout: 5000 }).catch(() => null);
		if (email) {
			await page.fill('#username', 'admin');
			await page.fill('#email', 'admin@example.com');
			await page.fill('#password', 'correct horse battery');
			await page.click('button[type=submit]');
			await page.waitForURL(/\/(?:site-admin\/configuration|search)/);
		}
		await context.close();
	}

	{
		const context = await browser.newContext();
		const page = await context.newPage();
		await signIn(page);
		const r = await page.request.post(`${BASE}/.api/graphql`, {
			headers: { 'X-Requested-With': 'Sourcegull' },
			data: { query: '{ statusMessages { __typename ... on SyncError { message } ... on NoRepositoriesDetected { message } } }' },
		});
		console.log(`  status: ${await r.text()}`);
		await context.close();
	}

	for (const theme of ['light', 'dark']) await shots(browser, theme);

	step('recording: type a search');
	// Signed in beforehand, so the recording starts on the search page.
	const signedIn = await browser.newContext();
	const before = await signedIn.newPage();
	await signIn(before);
	// The searches above are in the recent searches list on the home page.
	await before.goto(`${BASE}/search`);
	const clear = await before.waitForSelector('button:has-text("Clear")', { timeout: 5000 }).catch(() => null);
	if (clear) {
		await clear.click();
		await before.waitForSelector('button:has-text("Clear")', { state: 'detached', timeout: 5000 });
	}
	const storageState = await signedIn.storageState();
	await signedIn.close();
	const context = await browser.newContext({
		viewport: VIEWPORT,
		colorScheme: 'light',
		storageState,
		deviceScaleFactor: 2,
	});
	const page = await context.newPage();
	await page.goto(`${BASE}/search`);
	await page.waitForSelector('.query-input .cm-content');
	await page.mouse.move(VIEWPORT.width / 2, VIEWPORT.height - 1);
	await page.waitForTimeout(500);
	// One screenshot per step, with its duration in frames.txt (ffmpeg's concat demuxer).
	const dir = `${OUT}/frames`;
	fs.rmSync(dir, { recursive: true, force: true });
	fs.mkdirSync(dir, { recursive: true });
	let list = '';
	let n = 0;
	const frame = async (seconds) => {
		const name = `${String(n++).padStart(4, '0')}.png`;
		await page.screenshot({ path: `${dir}/${name}` });
		list += `file '${name}'\nduration ${seconds}\n`;
	};
	await page.click('.query-input .cm-content');
	await frame(0.8);
	for (const ch of QUERY) {
		await page.keyboard.type(ch);
		await page.waitForTimeout(30);
		await frame(0.08);
	}
	await page.keyboard.press('Escape');
	await page.waitForTimeout(200);
	await frame(0.4);
	await page.keyboard.press('Enter');
	await results(page);
	await frame(3);
	// The concat demuxer ignores the last entry's duration unless the file is listed again.
	list += `file '${String(n - 1).padStart(4, '0')}.png'\n`;
	fs.writeFileSync(`${dir}/frames.txt`, list);
	console.log(`  ${n} frames`);
	await context.close();
	await browser.close();
})().catch((e) => {
	console.error(e);
	process.exit(1);
});
