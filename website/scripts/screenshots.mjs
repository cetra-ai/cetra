// Rewrites the docs' UI screenshots from the web UI running on its MSW mocks (D-W-12).
// Usage: node scripts/screenshots.mjs [shot name ...]
// Needs Chromium: `pnpm --dir website exec playwright install chromium` once.
import { spawn } from "node:child_process";
import { mkdirSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright";

const SITE = dirname(dirname(fileURLToPath(import.meta.url)));
const ROOT = dirname(SITE);
const ORIGIN = "http://localhost:5199";
const OUT = join(SITE, "src/assets/screenshots");
// The page clock is pinned so each run draws the same pictures. The dev server serves the large
// generated dataset, built from the page's clock; its experiments start on 2026-11-02, so the
// default stands a week after them. A shot of a page still on the small fixtures (dated
// 2026-01-01, 10:00 to 10:59 UTC) sets its own `now` just after them.
const NOW = "2026-11-19T12:00:00Z";

async function startServer() {
  const server = spawn("pnpm", ["--dir", "web", "exec", "vite", "--port", "5199", "--strictPort"], {
    cwd: ROOT,
    stdio: ["ignore", "pipe", "pipe"],
  });
  let output = "";
  server.stdout.on("data", (chunk) => {
    output += chunk;
  });
  server.stderr.on("data", (chunk) => {
    output += chunk;
  });
  const deadline = Date.now() + 60_000;
  while (Date.now() < deadline) {
    if (server.exitCode !== null) break;
    try {
      if ((await fetch(`${ORIGIN}/`)).ok) return server;
    } catch {
      // not up yet
    }
    await new Promise((resolve) => setTimeout(resolve, 500));
  }
  server.kill();
  throw new Error(`the web dev server did not start:\n${output}`);
}

async function shoot(context, shot) {
  const page = await context.newPage();
  try {
    // Each shot starts from empty storage (no "Recent" items from an earlier shot). The docs show
    // the light theme; a marketing shot sets `"theme": "dark"`, the UI's default.
    const theme = shot.theme ?? "light";
    await page.emulateMedia({ colorScheme: theme });
    await page.addInitScript((value) => {
      try {
        localStorage.clear();
        localStorage.setItem("cetra.theme", value);
      } catch {
        // storage unavailable: the UI's default theme stays
      }
    }, theme);
    await page.clock.setFixedTime(new Date(shot.now ?? NOW));
    await page.goto(ORIGIN + shot.path);
    await page.getByText(shot.waitFor).first().waitFor();
    for (const action of shot.actions ?? []) {
      await page.locator(action.click).first().click();
      // `type` replaces the text of the element just clicked (an editor).
      if (action.type !== undefined) {
        await page.keyboard.press("ControlOrMeta+a");
        await page.keyboard.insertText(action.type);
      }
    }
    if (shot.waitAfter) await page.getByText(shot.waitAfter).first().waitFor();
    await page.waitForLoadState("networkidle");
    await page.evaluate(() => document.fonts.ready);
    // The SQL console shows how long the query took, measured by the browser's own timer
    // (not the page clock), so it moves from run to run. Pin it to a fixed value.
    await page.evaluate(() => {
      const walker = document.createTreeWalker(document.body, NodeFilter.SHOW_TEXT);
      for (let node = walker.nextNode(); node; node = walker.nextNode()) {
        node.textContent = node.textContent.replace(/^(\d+ rows? · )\d+ ms$/, "$127 ms");
      }
    });
    const path = join(OUT, `${shot.name}.png`);
    await page.screenshot({ path, fullPage: false, animations: "disabled", caret: "hide" });
    console.log(`wrote ${path}`);
  } finally {
    await page.close();
  }
}

const shots = JSON.parse(readFileSync(join(SITE, "screenshots.json"), "utf8"));
const names = process.argv.slice(2);
const selected = names.length > 0 ? shots.filter((shot) => names.includes(shot.name)) : shots;
if (selected.length !== (names.length || shots.length)) {
  throw new Error(`unknown shot name in: ${names.join(", ")}`);
}
mkdirSync(OUT, { recursive: true });

const server = await startServer();
let browser;
try {
  browser = await chromium.launch();
  const context = await browser.newContext({
    viewport: { width: 1440, height: 900 },
    deviceScaleFactor: 2,
    colorScheme: "light",
    reducedMotion: "reduce",
    locale: "en-US",
    timezoneId: "UTC",
  });
  // The first load registers the mock service worker; later pages are intercepted from the start.
  const warmup = await context.newPage();
  await warmup.goto(`${ORIGIN}/`);
  await warmup.evaluate(() => navigator.serviceWorker.ready);
  await warmup.close();
  for (const shot of selected) await shoot(context, shot);
} finally {
  await browser?.close();
  server.kill();
}
