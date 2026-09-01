// Drive the browser suite in real Chromium and report.
//
// Not a node smoke test on purpose. doc/Release.md's stated trigger for the
// wasm32 release leg is a node step asserting abiVersion() === 1, and that
// is a lower bar than it should be: node's event loop is not the browser's,
// and the one real browser-vs-native divergence this project has actually
// hit (doc/HostBaseline.md -- Lab's REPL cannot answer host.time() because
// it evaluates on a thread that cannot park) is invisible from node. The
// browser is the thing being shipped, so the browser is what runs the test.
import { chromium } from "playwright";
import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { extname, join, normalize } from "node:path";

const ROOT = new URL(".", import.meta.url).pathname;
const TYPES = {
  ".html": "text/html", ".mjs": "text/javascript", ".js": "text/javascript",
  ".wasm": "application/wasm", ".json": "application/json",
};

// A real server, not file://: wasm instantiation from file:// is blocked by
// the same-origin rules, and a page that only works under a server is the
// page a release actually ships.
const server = createServer(async (req, res) => {
  try {
    const rel = normalize(decodeURI(req.url.split("?")[0])).replace(/^(\.\.[/\\])+/, "");
    const path = join(ROOT, rel === "/" ? "index.html" : rel);
    const body = await readFile(path);
    res.writeHead(200, { "content-type": TYPES[extname(path)] || "application/octet-stream" });
    res.end(body);
  } catch {
    res.writeHead(404).end("not found");
  }
});
await new Promise((r) => server.listen(0, r));
const port = server.address().port;

// The environment's preinstalled Chromium, not one Playwright downloads.
// PLAYWRIGHT_SKIP_BROWSER_DOWNLOAD is set here and the npm package's pinned
// browser build may not match what is on disk; pointing at the binary is
// what the harness documents for exactly this case. CHROMIUM_PATH lets CI
// override it.
const executablePath =
  process.env.CHROMIUM_PATH || "/opt/pw-browsers/chromium-1194/chrome-linux/chrome";
const browser = await chromium.launch({ executablePath });
const page = await browser.newPage();
const consoleErrors = [];
page.on("console", (m) => { if (m.type() === "error") consoleErrors.push(m.text()); });
page.on("pageerror", (e) => consoleErrors.push(String(e)));

await page.goto(`http://127.0.0.1:${port}/`);
await page.waitForFunction(() => window.__drtResults !== undefined, { timeout: 30_000 });
const results = await page.evaluate(() => window.__drtResults);

await browser.close();
server.close();

let failed = 0;
for (const r of results) {
  console.log(`${r.ok ? "  ok  " : "FAILED"}  ${r.name}${r.ok ? "" : "\n          " + r.why}`);
  if (!r.ok) failed++;
}
console.log(`\n${results.length - failed} passed, ${failed} failed, in real Chromium`);
if (consoleErrors.length) {
  console.log("\nconsole errors:");
  for (const e of consoleErrors) console.log("  " + e);
}
process.exit(failed ? 1 : 0);
