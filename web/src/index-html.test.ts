import { expect, it } from "vitest";
import html from "../index.html?raw";

it("declares both colour schemes, so browsers do not darken the page themselves", () => {
  expect(html).toContain('<meta name="color-scheme" content="light dark" />');
});

it("locks Dark Reader, since the app has its own dark theme", () => {
  expect(html).toContain('<meta name="darkreader-lock" />');
});
