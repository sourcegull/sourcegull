// Browser check of code navigation in the file view: hover an
// identifier, go to its definition (button and Ctrl-click), find its references in the
// usages panel, and the phone layout. Fails on any page or console error. Screenshots go to OUT.
//
// Run it with scripts/test/e2e-ui.sh, after ui/e2e/search.cjs has signed up the admin and with
// the repository it sets up (the Sourcegull sources).
const { chromium } = require('playwright');
const BASE = process.env.BASE || 'http://127.0.0.1:7090';
const OUT = process.env.OUT || require('node:os').tmpdir();
const REPO = 'gitlab.example.com/acme/sourcegull';
const FILE = `${BASE}/${REPO}/-/blob/crates/search/src/chunks.rs`;

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
	const fail = (msg) => {
		throw new Error(msg);
	};

	// The screen rectangle of the first `word` on the code line containing `lineText`.
	const wordBox = (lineText, word) =>
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
	const hover = async (lineText, word) => {
		await page.mouse.move(5, 5);
		await page.waitForSelector('.cm-codenav-hover', { state: 'detached', timeout: 5000 }).catch(() => {});
		const box = await wordBox(lineText, word);
		if (!box) fail(`no ${word} on the line ${lineText}`);
		await page.mouse.move(box.x, box.y);
		await page.waitForSelector('.cm-codenav-hover', { timeout: 15000 });
		return box;
	};

	// Opens the file at a line and waits until the view has scrolled to it.
	const openAt = async (line, text) => {
		await page.goto(`${FILE}#L${line}`);
		await page.waitForSelector(`.cm-selected-line:has-text(${JSON.stringify(text)})`, { timeout: 30000 });
		await page.waitForTimeout(300);
	};

	step('sign in');
	await page.goto(`${BASE}/sign-in`);
	await page.fill('#username', 'admin');
	await page.fill('#password', 'correct horse battery');
	await page.click('button[type=submit]');
	await page.waitForURL((url) => !url.pathname.startsWith('/sign-in'));

	step('hover shows the definition and its doc comment');
	await openAt(215, 'let l = locate(&newlines');
	await hover('let l = locate(&newlines', 'locate');
	const card = await page.textContent('.cm-codenav-hover');
	if (!card.includes('pub fn locate(newlines: &[u32], offset: usize) -> Location {')) fail(`hover: ${card}`);
	if (!card.includes('The location of byte')) fail(`no doc comment in the hover: ${card}`);
	if (!card.includes('Search-based')) fail(`no provenance badge: ${card}`);
	await page.screenshot({ path: `${OUT}/codenav-01-hover.png` });

	step('find references opens the usages panel');
	await page.click('.cm-codenav-hover button:has-text("Find references")');
	await page.waitForSelector('.usages .line', { timeout: 30000 });
	const panel = await page.textContent('.usages');
	if (!panel.includes('References to locate')) fail(`panel heading: ${panel}`);
	if (!/Definitions\s*\(\d+\)/.test(panel)) fail(`no definitions section: ${panel}`);
	const lines = await page.$$eval('.usages .line', (els) => els.map((e) => e.textContent.trim()));
	if (!lines.some((l) => l.includes('pub fn locate(newlines'))) fail(`no definition line: ${lines}`);
	if (!lines.some((l) => l.includes('let l = locate(&newlines, o)'))) fail(`no reference line: ${lines}`);
	await page.screenshot({ path: `${OUT}/codenav-02-references.png` });

	step('locals resolve syntactically, within the file');
	await page.click('.usages button:has-text("Close")');
	await hover('let l = locate(&newlines', 'newlines');
	const local = await page.textContent('.cm-codenav-hover');
	if (!local.includes('Syntactic')) fail(`a local is not syntactic: ${local}`);
	if (!local.includes('let newlines')) fail(`local hover: ${local}`);
	await page.click('.cm-codenav-hover button:has-text("Find references")');
	await page.waitForFunction(() => document.querySelector('.usages h2')?.textContent?.includes('newlines'));
	await page.waitForSelector('.usages .line', { timeout: 30000 });
	const localLines = await page.$$eval('.usages .line', (els) => els.map((e) => e.textContent.trim()));
	if (localLines.length !== 2) fail(`local usages: ${localLines}`);
	const files = await page.$$eval('.usages .file-name', (els) => els.map((e) => e.textContent.trim()));
	if (files.some((f) => !f.endsWith('crates/search/src/chunks.rs'))) fail(`local usages in other files: ${files}`);
	await page.screenshot({ path: `${OUT}/codenav-02b-local.png` });
	await page.click('.usages button:has-text("Close")');
	await hover('let l = locate(&newlines', 'locate');
	await page.click('.cm-codenav-hover button:has-text("Find references")');
	await page.waitForFunction(() => document.querySelector('.usages h2')?.textContent?.includes('locate'));
	await page.waitForSelector('.usages .line:has-text("pub fn locate(newlines")', { timeout: 30000 });

	step('a usage opens its line');
	await page.click('.usages .line:has-text("pub fn locate(newlines")');
	await page.waitForURL(/chunks\.rs#L42:8$/, { timeout: 10000 });
	await page.waitForSelector('.cm-selected-line:has-text("pub fn locate")', { timeout: 10000 });

	step('go to definition from the hover');
	await openAt(215, 'let l = locate(&newlines');
	await hover('let l = locate(&newlines', 'locate');
	await page.click('.cm-codenav-hover a:has-text("Go to definition")');
	await page.waitForURL(/chunks\.rs#L42:8$/, { timeout: 10000 });
	await page.waitForSelector('.cm-selected-line:has-text("pub fn locate")', { timeout: 10000 });

	step('Ctrl-click goes to the definition');
	await openAt(215, 'let l = locate(&newlines');
	const box = await wordBox('let l = locate(&newlines', 'locate');
	await page.keyboard.down('Control');
	await page.mouse.click(box.x, box.y);
	await page.keyboard.up('Control');
	await page.waitForURL(/chunks\.rs#L42:8$/, { timeout: 15000 });

	step('keywords have no hover');
	await openAt(42, 'pub fn locate(newlines');
	const kw = await wordBox('pub fn locate(newlines', 'pub');
	await page.mouse.move(kw.x, kw.y);
	await page.waitForTimeout(1500);
	if (await page.$('.cm-codenav-hover')) fail('a keyword got a hover');

	step('phone layout');
	await page.setViewportSize({ width: 390, height: 844 });
	await openAt(215, 'let l = locate');
	await hover('let l = locate', 'locate');
	await page.screenshot({ path: `${OUT}/codenav-03-phone-hover.png` });
	await page.click('.cm-codenav-hover button:has-text("Find references")');
	await page.waitForSelector('.usages .line', { timeout: 30000 });
	const overflow = await page.evaluate(() => document.documentElement.scrollWidth - window.innerWidth);
	if (overflow > 1) fail(`horizontal page scroll on a phone: ${overflow}px`);
	await page.screenshot({ path: `${OUT}/codenav-04-phone-references.png` });

	if (problems.length) fail(problems.join('\n'));
	await browser.close();
	console.log('ok');
})().catch((e) => {
	console.error(e);
	process.exit(1);
});
