// Vitest runs in Node. The app tsconfig only includes the DOM types, so the
// builtins are suppressed here, as in contrast.test.ts.
// @ts-expect-error node builtin, available under Vitest
import { readdirSync, readFileSync } from "node:fs";
// @ts-expect-error node builtin, available under Vitest
import { dirname, join, relative } from "node:path";
// @ts-expect-error node builtin, available under Vitest
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

// Guards the named type scale (src/styles.css, @theme): code uses only
// text-caption, text-body, text-ui, text-title, text-heading and text-display.
const srcRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const self = fileURLToPath(import.meta.url);
const forbidden = /\btext-(xs|sm|base|lg|[2-9]?xl)\b|\btext-\[\d+(\.\d+)?px\]/g;

function sourceFiles(dir: string): string[] {
  return readdirSync(dir, { withFileTypes: true }).flatMap(
    (entry: { name: string; isDirectory(): boolean }) => {
      const path = join(dir, entry.name);
      if (entry.isDirectory()) return sourceFiles(path);
      if (!/\.tsx?$/.test(entry.name) || path === self) return [];
      return [path];
    },
  );
}

describe("type scale", () => {
  it("uses only the named text sizes in src/", () => {
    const hits: string[] = [];
    for (const file of sourceFiles(srcRoot)) {
      readFileSync(file, "utf8")
        .split("\n")
        .forEach((line: string, index: number) => {
          for (const match of line.matchAll(forbidden)) {
            hits.push(`${relative(srcRoot, file)}:${index + 1}: ${match[0]}`);
          }
        });
    }
    expect(hits, `Use the named type scale instead:\n${hits.join("\n")}`).toEqual([]);
  });
});
