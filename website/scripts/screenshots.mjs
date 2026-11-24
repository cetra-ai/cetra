// Rewrites the docs' UI screenshots from the web UI running on its MSW mocks (D-W-12).
// Usage: [CETRA_SCREENSHOTS_PORT=port] node scripts/screenshots.mjs [shot name ...]
// The dev server uses port 5199 unless CETRA_SCREENSHOTS_PORT says otherwise.
// Needs Chromium: `pnpm --dir website exec playwright install chromium` once.
import { spawn } from "node:child_process";
import { mkdirSync, readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright";
import { portIsFree } from "./port-free.mjs";
import { checkClipRect, checkStorage } from "./screenshot-storage.mjs";

const SITE = dirname(dirname(fileURLToPath(import.meta.url)));
const ROOT = dirname(SITE);
const PORT = Number(process.env.CETRA_SCREENSHOTS_PORT ?? 5199);
const ORIGIN = `http://localhost:${PORT}`;
const OUT = join(SITE, "src/assets/screenshots");
// The page clock is pinned so each run draws the same pictures. The dev server serves the large
// generated dataset, built from the page's clock; its experiments start on 2026-11-02, so the
// default stands a week after them. A shot of a page still on the small fixtures (dated
// 2026-01-01, 10:00 to 10:59 UTC) sets its own `now` just after them.
const NOW = "2026-11-19T12:00:00Z";

async function startServer() {
  // Another server on the port would answer the poll below and every shot would come from its
  // (possibly stale) UI, so refuse to start.
  if (!(await portIsFree(PORT))) {
    throw new Error(
      `port ${PORT} is already in use (a vite dev server from another worktree?); stop that process and rerun`,
    );
  }
  const server = spawn(
    "pnpm",
    ["--dir", "web", "exec", "vite", "--port", String(PORT), "--strictPort"],
    {
      cwd: ROOT,
      stdio: ["ignore", "pipe", "pipe"],
    },
  );
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
  // A marketing shot sets its own window size; the docs' shots keep the context's 1440×900.
  if (shot.viewport) await page.setViewportSize(shot.viewport);
  try {
    // Each shot starts from empty storage (no "Recent" items from an earlier shot). The docs show
    // the light theme; a marketing shot sets `"theme": "dark"`, the UI's default. A shot's
    // `storage` then sets its own keys, after the theme, so it can override `cetra.theme` too.
    const theme = shot.theme ?? "light";
    await page.emulateMedia({ colorScheme: theme });
    await page.addInitScript(
      ({ theme, storage }) => {
        try {
          localStorage.clear();
          localStorage.setItem("cetra.theme", theme);
          for (const [key, value] of Object.entries(storage)) localStorage.setItem(key, value);
        } catch {
          // storage unavailable: the UI's defaults stay
        }
      },
      { theme, storage: shot.storage ?? {} },
    );
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
    const options = { path, animations: "disabled", caret: "hide" };
    // `clip` captures one element (the panel a section talks about) instead of the window;
    // `clipRect` captures a rectangle of the window in CSS px (a window over the page).
    if (shot.clip) await page.locator(shot.clip).first().screenshot(options);
    else if (shot.clipRect) await page.screenshot({ ...options, clip: shot.clipRect });
    else await page.screenshot({ ...options, fullPage: false });
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
checkStorage(selected);
checkClipRect(selected);
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
