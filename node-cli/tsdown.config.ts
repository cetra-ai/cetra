import { defineConfig } from "tsdown";

export default defineConfig({
  entry: ["src/cli.ts"],
  format: "esm",
  platform: "node",
  target: "node22",
  dts: false,
  clean: true,
  // `platform: "node"` would name the file cli.mjs; the package is `type: module`, so .js is right.
  fixedExtension: false,
  copy: [{ from: "../skills/cetra", to: "dist/skills" }],
});
