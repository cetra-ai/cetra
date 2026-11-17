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
// The fixtures are on 2026-01-01 between 10:00 and 10:59 UTC; the clock stands just after.
const NOW = new Date("2026-01-01T11:00:00Z");

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
    await page.clock.setFixedTime(NOW);
    await page.goto(ORIGIN + shot.path);
    await page.getByText(shot.waitFor).first().waitFor();
    for (const action of shot.actions ?? []) {
      await page.locator(action.click).first().click();
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
  // The UI defaults to the dark theme; the docs show the light one.
  await context.addInitScript(() => {
    try {
      localStorage.setItem("cetra.theme", "light");
    } catch {
      // storage unavailable: the default theme stays
    }
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
