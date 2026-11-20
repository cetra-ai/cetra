// Renders public/og.png, the Open Graph image of every marketing page (1200x630).
// Usage: pnpm --dir website exec node scripts/og-image.mjs
// Needs Chromium: `pnpm --dir website exec playwright install chromium` once.
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { chromium } from "playwright";

const SITE = dirname(dirname(fileURLToPath(import.meta.url)));
const OUT = join(SITE, "public/og.png");

// Chromium blocks file:// fonts in a setContent page, so the fonts are inlined as data URIs.
const font = (family, file, weight) => {
  const data = readFileSync(join(SITE, "node_modules", file)).toString("base64");
  return `@font-face { font-family: "${family}"; src: url(data:font/woff2;base64,${data}) format("woff2"); font-weight: ${weight}; }`;
};
const FONTS = [
  font(
    "Space Grotesk",
    "@fontsource-variable/space-grotesk/files/space-grotesk-latin-wght-normal.woff2",
    "300 700",
  ),
  font(
    "JetBrains Mono",
    "@fontsource-variable/jetbrains-mono/files/jetbrains-mono-latin-wght-normal.woff2",
    "100 800",
  ),
].join("\n");

const logo = readFileSync(
  join(SITE, "src/marketing/assets/cetra-logo-on-dark.svg"),
  "utf8",
).replace("<svg ", '<svg style="height:56px;width:auto" ');

const html = `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8" />
<style>
${FONTS}
html, body { margin: 0; width: 1200px; height: 630px; background: #141414; }
body { display: flex; flex-direction: column; justify-content: space-between; padding: 72px 80px; box-sizing: border-box; }
h1 { font: 600 68px/1.05 "Space Grotesk"; letter-spacing: -0.03em; color: #f5f5f4; margin: 0; max-width: 900px; }
.tagline { font: 400 30px/1.3 "Space Grotesk"; color: #a8a29e; margin: 20px 0 0; }
.domain { font: 500 24px "JetBrains Mono"; color: #2dd4bf; margin: 0; }
svg { align-self: flex-start; }
</style>
</head>
<body>
${logo}
<div>
<h1>LLM observability you run yourself.</h1>
<p class="tagline">One binary on your server, or cetra Cloud.</p>
</div>
<p class="domain">cetra.sh</p>
</body>
</html>`;

const browser = await chromium.launch();
try {
  const page = await browser.newPage({
    viewport: { width: 1200, height: 630 },
    deviceScaleFactor: 1,
  });
  await page.setContent(html);
  await page.evaluate(() => document.fonts.ready);
  const loaded = await page.evaluate(() => document.fonts.check('600 68px "Space Grotesk"'));
  if (!loaded) throw new Error("font did not load");
  await page.screenshot({ path: OUT, type: "png" });
} finally {
  await browser.close();
}
console.log("wrote public/og.png");
