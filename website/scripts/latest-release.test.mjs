import assert from "node:assert/strict";
import { test } from "node:test";
import { latestRelease } from "./latest-release.mjs";

test("is null before the first release", () => {
  assert.equal(latestRelease("# Changelog\n\n## [Unreleased]\n\n### Added\n"), null);
});

test("is the first released heading", () => {
  const md = "## [Unreleased]\n\n## [0.2.0] - 2027-01-02\n\n## [0.1.0] - 2026-12-01\n";
  assert.equal(latestRelease(md), "0.2.0");
});
