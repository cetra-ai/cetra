/**
 * The site's TypeScript API page names every export of every entry point and
 * nothing else (D-W-8). The exports come from the TypeScript compiler, values
 * and types alike.
 */

import { existsSync, readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import ts from "typescript";
import { describe, expect, it } from "vitest";

const PAGE = fileURLToPath(
  new URL("../../../website/src/content/docs/docs/reference/typescript/api.mdx", import.meta.url),
);
const PACKAGE = new URL("../package.json", import.meta.url);

// Entry point (the page's `##` heading) to its source file in src/.
const ENTRIES: Record<string, string> = {
  "@cetra-ai/sdk": "./index.ts",
  "@cetra-ai/sdk/openai": "./openai.ts",
  "@cetra-ai/sdk/anthropic": "./anthropic.ts",
  "@cetra-ai/sdk/langchain": "./langchain.ts",
  "@cetra-ai/sdk/browser": "./browser.ts",
};

/** The sorted names exported by one source file, values and types. */
function exportsOf(file: string): string[] {
  const path = fileURLToPath(new URL(file, import.meta.url));
  const program = ts.createProgram([path], {
    strict: true,
    module: ts.ModuleKind.NodeNext,
    moduleResolution: ts.ModuleResolutionKind.NodeNext,
    skipLibCheck: true,
  });
  const checker = program.getTypeChecker();
  const source = program.getSourceFile(path);
  if (source === undefined) {
    throw new Error(`no source ${file}`);
  }
  const symbol = checker.getSymbolAtLocation(source);
  if (symbol === undefined) {
    return [];
  }
  return checker
    .getExportsOfModule(symbol)
    .map((exported) => exported.getName())
    .sort();
}

/**
 * Split the page on its `## \`<entry>\`` headings and collect the
 * `### \`<name>\`` headings under each. A name documented twice under one
 * entry point is kept twice, so the comparison fails on it.
 */
function documented(markdown: string): Map<string, string[]> {
  const found = new Map<string, string[]>();
  let current: string[] | undefined;
  for (const line of markdown.split("\n")) {
    const entry = /^## `([^`]+)`$/.exec(line);
    if (entry !== null) {
      current = [];
      found.set(entry[1] ?? "", current);
      continue;
    }
    const name = /^### `([A-Za-z_$][A-Za-z0-9_$]*)`$/.exec(line);
    if (name !== null && current !== undefined) {
      current.push(name[1] ?? "");
    }
  }
  return found;
}

describe("API reference page", () => {
  it("lists the entry points of package.json", () => {
    const pkg = JSON.parse(readFileSync(PACKAGE, "utf8")) as { exports: Record<string, unknown> };
    const subpaths = Object.keys(pkg.exports).map((key) =>
      key === "." ? "@cetra-ai/sdk" : `@cetra-ai/sdk/${key.slice(2)}`,
    );
    expect(subpaths.sort()).toEqual(Object.keys(ENTRIES).sort());
  });

  it.skipIf(!existsSync(PAGE))(
    "documents every export of every entry point, and nothing else",
    () => {
      const page = documented(readFileSync(PAGE, "utf8"));
      expect([...page.keys()].sort()).toEqual(Object.keys(ENTRIES).sort());
      for (const [entry, file] of Object.entries(ENTRIES)) {
        expect(page.get(entry)?.slice().sort(), entry).toEqual(exportsOf(file));
      }
    },
    30_000,
  );
});
