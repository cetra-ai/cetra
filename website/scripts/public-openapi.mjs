// Writes src/generated/openapi.json: crates/server/openapi.json without the control
// plane's `internal` operations (D-W-7). The generated API reference renders this file.
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";
import { fileURLToPath } from "node:url";

const METHODS = ["get", "put", "post", "delete", "options", "head", "patch", "trace"];

function isInternal(operation) {
  return Array.isArray(operation?.tags) && operation.tags.includes("internal");
}

export function publicSpec(spec) {
  const copy = structuredClone(spec);
  const paths = {};
  for (const [path, item] of Object.entries(copy.paths ?? {})) {
    for (const method of METHODS) {
      if (isInternal(item[method])) delete item[method];
    }
    if (METHODS.some((method) => item[method])) paths[path] = item;
  }
  copy.paths = paths;
  if (copy.tags) copy.tags = copy.tags.filter((tag) => tag.name !== "internal");
  if (!copy.servers || copy.servers.length === 0) {
    copy.servers = [
      { url: "http://localhost:8080", description: "A local cetra; use your instance's URL" },
    ];
  }
  return copy;
}

function countOperations(spec) {
  let count = 0;
  for (const item of Object.values(spec.paths ?? {})) {
    count += METHODS.filter((method) => item[method]).length;
  }
  return count;
}

function main() {
  const source = new URL("../../crates/server/openapi.json", import.meta.url);
  const target = new URL("../src/generated/openapi.json", import.meta.url);
  const spec = JSON.parse(readFileSync(source, "utf8"));
  const result = publicSpec(spec);
  mkdirSync(dirname(fileURLToPath(target)), { recursive: true });
  writeFileSync(target, `${JSON.stringify(result, null, 2)}\n`);
  const kept = countOperations(result);
  const dropped = countOperations(spec) - kept;
  console.log(`public-openapi: ${kept} operations kept, ${dropped} dropped`);
}

if (process.argv[1] === fileURLToPath(import.meta.url)) main();
