import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { test } from "node:test";

const tokens = readFileSync(new URL("./tokens.css", import.meta.url), "utf8");

/**
 * The raw palette: `--c-*` name to hex, from the `:root` palette lines.
 * @param {string} css
 * @returns {Map<string, string>}
 */
function parseTokens(css) {
  const map = new Map();
  for (const line of css.split("\n")) {
    const m = line.match(/^\s*(--c-[a-z0-9-]+):\s*(#[0-9a-f]{6});/i);
    if (m) map.set(m[1], m[2]);
  }
  return map;
}

const palette = parseTokens(tokens);

/** @param {string} ref a `--c-*` name or a `#rrggbb` literal */
function hexOf(ref) {
  if (ref.startsWith("--")) {
    const hex = palette.get(ref);
    assert.ok(hex, `unknown palette token ${ref}`);
    return hex;
  }
  return ref;
}

/** @param {string} hex */
function luminance(hex) {
  const [r, g, b] = [1, 3, 5].map((i) => {
    const c = Number.parseInt(hex.slice(i, i + 2), 16) / 255;
    return c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4;
  });
  return 0.2126 * r + 0.7152 * g + 0.0722 * b;
}

/** @param {string} a @param {string} b */
function ratio(a, b) {
  const la = luminance(a);
  const lb = luminance(b);
  return (Math.max(la, lb) + 0.05) / (Math.min(la, lb) + 0.05);
}

const pairs = [
  ["--c-paper", "--c-ink"],
  ["--c-stone-400", "--c-ink"],
  ["--c-paper", "--c-ink-2"],
  ["--c-stone-400", "--c-ink-2"],
  ["--c-stone-400", "--c-ink-3"],
  ["--c-teal-light", "--c-ink"],
  ["--c-teal-ink", "--c-teal-light"],
  ["--c-paper", "--c-teal-deep"],
  ["#a7d8d1", "--c-teal-deep"],
  ["--c-ink", "--c-paper"],
  ["--c-stone-600", "--c-paper"],
  ["--c-teal", "--c-paper"],
  ["#ffffff", "--c-teal"],
  ["--c-teal-light", "--c-ink-2"],
  ["--c-teal-light", "--c-ink-3"],
  ["--c-teal-light", "--c-teal-deep"],
  ["--c-teal-light", "#0f3b37"],
  ["--c-paper", "#0f3b37"],
  ["#a7d8d1", "#0f3b37"],
  ["#a7d8d1", "#134843"],
  ["--c-stone-600", "#ecebe9"],
  ["--c-teal", "#ffffff"],
  ["--c-stone-300", "--c-ink-0"],
  ["--c-paper", "--c-ink-0"],
  ["--c-teal-light", "--c-ink-0"],
  ["--c-stone-400", "--c-ink-0"],
  ["--c-paper", "#2a2a2a"],
  ["--c-stone-400", "#2a2a2a"],
  ["--c-code-string", "--c-ink-2"],
  ["--c-code-constant", "--c-ink-2"],
  ["--c-code-function", "--c-ink-2"],
  ["--c-stone-300", "--c-ink-2"],
];

test("text pairs meet 4.5:1", () => {
  for (const [fg, bg] of pairs) {
    const r = ratio(hexOf(fg), hexOf(bg));
    assert.ok(r >= 4.5, `${fg} on ${bg} is ${r.toFixed(2)}:1, below 4.5:1`);
  }
});

test("every palette name in the table exists", () => {
  for (const [fg, bg] of pairs) {
    for (const ref of [fg, bg]) {
      if (ref.startsWith("--")) assert.ok(palette.has(ref), `missing ${ref}`);
    }
  }
});

test("luminance of black and white", () => {
  assert.equal(luminance("#000000"), 0);
  assert.equal(luminance("#ffffff"), 1);
  assert.ok(Math.abs(ratio("#000000", "#ffffff") - 21) < 1e-9);
});
