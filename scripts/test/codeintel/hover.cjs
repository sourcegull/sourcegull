// Browser part of the code intelligence test (scripts/test/codeintel.sh): in the file view of an auto-indexed repository, the hover over a reference shows the precise definition's
// docs, "Go to definition" opens the other file, and "Find references" lists precise usages in
// other files. Runs in the e2e image (Playwright, Chromium) on the dev network.
const { chromium } = require('playwright');
const BASE = process.env.BASE || 'http://sourcegull-k3s:30080';
const OUT = process.env.OUT || '/src/.test-out/codeintel';
const REPO = process.env.REPO || 'gitlab.localhost:8929/codeintel/auto-python';
const FILE = `${BASE}/${REPO}/-/blob/src/itsdangerous/timed.py`;
const LINE = 'class TimestampSigner(Signer):';

let page;
(async () => {
	const browser = await chromium.launch({ executablePath: process.env.CHROMIUM_EXECUTABLE });
	page = await (await browser.newContext({ viewport: { width: 1400, height: 900 } })).newPage();
	const problems = [];
	page.on('pageerror', (e) => problems.push(`pageerror: ${e.message}`));
	const fail = (msg) => {
		throw new Error(msg);
	};

	await page.goto(`${BASE}/sign-in`);
	await page.fill('#username', 'admin');
	await page.fill('#password', 'codeintel-test-Pw-7x');
	await page.click('button[type=submit]');
	await page.waitForURL((url) => !url.pathname.startsWith('/sign-in'));

	await page.goto(`${FILE}#L22`);
	await page.waitForSelector(`.cm-selected-line:has-text(${JSON.stringify(LINE)})`, { timeout: 30000 });
	await page.waitForTimeout(300);
	// The middle of the second word "Signer" on the line (the first is in "TimestampSigner").
	const box = await page.evaluate((lineText) => {
		const line = [...document.querySelectorAll('.cm-line')].find((l) => l.textContent.includes(lineText));
		const start = line.textContent.indexOf('(Signer') + 1;
		const walker = document.createTreeWalker(line, NodeFilter.SHOW_TEXT);
		let seen = 0;
		for (let n = walker.nextNode(); n; n = walker.nextNode()) {
			if (start < seen + n.textContent.length) {
				const r = document.createRange();
				r.setStart(n, start - seen);
				r.setEnd(n, start - seen + 1);
				const b = r.getBoundingClientRect();
				return { x: b.right + 4, y: b.top + b.height / 2 };
			}
			seen += n.textContent.length;
		}
		return null;
	}, LINE);
	if (!box) fail('no Signer on the line');
	await page.mouse.move(box.x, box.y);
	await page.waitForSelector('.cm-codenav-hover', { timeout: 15000 });
	const card = await page.textContent('.cm-codenav-hover');
	if (!card.includes('A signer securely signs bytes')) fail(`hover without the docstring: ${card}`);
	// scip-python's Markdown escapes (secret\_key) and &nbsp; indentation are resolved.
	if (/&nbsp;|\\_/.test(card) || !card.includes(':param secret_key:')) fail(`unrendered Markdown: ${card}`);
	if (!/precise/i.test(card)) console.log(`note: the hover card does not name its provenance: ${card.slice(0, 200)}`);
	await page.screenshot({ path: `${OUT}/hover-01-card.png` });
	console.log('- hover shows the precise docstring');

	await page.click('.cm-codenav-hover button:has-text("Find references")');
	await page.waitForSelector('.usages .line', { timeout: 30000 });
	const usages = await page.textContent('.usages');
	for (const f of ['serializer.py', '__init__.py']) {
		if (!usages.includes(f)) fail(`references lack ${f}: ${usages.slice(0, 500)}`);
	}
	await page.screenshot({ path: `${OUT}/hover-02-references.png` });
	console.log('- references list other files');
	await page.click('.usages button:has-text("Close")');

	await page.mouse.move(5, 5);
	await page.waitForSelector('.cm-codenav-hover', { state: 'detached', timeout: 5000 }).catch(() => {});
	await page.mouse.move(box.x, box.y);
	await page.waitForSelector('.cm-codenav-hover', { timeout: 15000 });
	await page.click('.cm-codenav-hover a:has-text("Go to definition")');
	await page.waitForURL(/signer\.py#L76/, { timeout: 15000 });
	await page.waitForSelector('.cm-selected-line:has-text("class Signer")', { timeout: 15000 });
	await page.screenshot({ path: `${OUT}/hover-03-definition.png` });
	console.log('- go to definition opens signer.py');

	if (problems.length) fail(problems.join('\n'));
	await browser.close();
	console.log('ok');
})().catch(async (e) => {
	console.error(e);
	if (page) {
		console.error(`at ${page.url()}`);
		await page.screenshot({ path: `${OUT}/hover-failure.png` }).catch(() => {});
	}
	process.exit(1);
});
