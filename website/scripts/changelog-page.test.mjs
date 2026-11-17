import assert from "node:assert/strict";
import { test } from "node:test";
import { changelogPage } from "./changelog-page.mjs";

test("builds the page from a small changelog", () => {
  const page = changelogPage(
    "# Changelog\n\nSee [the guide](docs/guide.md) and [site](https://cetra.sh/).\n\n## [Unreleased]\n\n### Added\n\n- A thing ([notes](#x)).\n",
  );
  assert.equal(
    page,
    [
      "---",
      "title: Changelog",
      "description: Every user-visible change to cetra, release by release.",
      "---",
      "",
      "<!-- Generated from CHANGELOG.md by scripts/changelog-page.mjs. Do not edit. -->",
      "",
      "See [the guide](https://github.com/cetra-ai/cetra/blob/main/docs/guide.md) and [site](https://cetra.sh/).",
      "",
      "## [Unreleased]",
      "",
      "### Added",
      "",
      "- A thing ([notes](#x)).",
      "",
    ].join("\n"),
  );
});
