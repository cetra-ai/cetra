import assert from "node:assert/strict";
import { describe, it } from "node:test";
import { checkPage, listedRoutes, pageWeight } from "./check-marketing.mjs";

// A valid page built from parts. Each part can be replaced by a test.
function page({
  htmlAttrs = 'lang="en"',
  title = "<title>T</title>",
  description = '<meta name="description" content="D">',
  canonical = '<link rel="canonical" href="https://cetra.sh/x/">',
  robots = '<meta name="robots" content="noindex">',
  body = "<h1>A</h1>",
} = {}) {
  return `<!doctype html><html ${htmlAttrs}><head>${title}${description}${canonical}${robots}</head><body>${body}</body></html>`;
}

const noindex = { noindex: true };

describe("checkPage", () => {
  it("a valid page has no findings", () => {
    assert.deepEqual(checkPage("/x/", page(), noindex), []);
  });

  it("attributes in another order and data-astro-cid attributes are fine", () => {
    const html = page({
      htmlAttrs: 'data-astro-cid-x lang="en"',
      description: '<meta content="D" name="description" data-astro-cid-y>',
      canonical: '<link href="https://cetra.sh/x/" rel="canonical">',
      body: '<h1 class="astro-abc">A</h1>',
    });
    assert.deepEqual(checkPage("/x/", html, noindex), []);
  });

  it("missing lang", () => {
    assert.deepEqual(checkPage("/x/", page({ htmlAttrs: "" }), noindex), ['missing lang="en"']);
  });

  it("empty title", () => {
    assert.deepEqual(checkPage("/x/", page({ title: "<title></title>" }), noindex), [
      "title missing",
    ]);
  });

  it("missing description", () => {
    assert.deepEqual(checkPage("/x/", page({ description: "" }), noindex), ["description missing"]);
  });

  it("description of 161 characters", () => {
    const description = `<meta name="description" content="${"x".repeat(161)}">`;
    assert.deepEqual(checkPage("/x/", page({ description }), noindex), [
      "description is 161 characters, the limit is 160",
    ]);
  });

  it("description of 160 characters passes, and entities count as one character each", () => {
    const exact = `<meta name="description" content="${"x".repeat(160)}">`;
    assert.deepEqual(checkPage("/x/", page({ description: exact }), noindex), []);
    const entities = `<meta name="description" content="&amp;&#x27;${"x".repeat(158)}">`;
    assert.deepEqual(checkPage("/x/", page({ description: entities }), noindex), []);
  });

  it("wrong canonical", () => {
    const canonical = '<link rel="canonical" href="https://cetra.sh/y/">';
    assert.deepEqual(checkPage("/x/", page({ canonical }), noindex), [
      "canonical must be https://cetra.sh/x/",
    ]);
  });

  it("noindex expected", () => {
    assert.deepEqual(checkPage("/x/", page({ robots: "" }), noindex), ["noindex expected"]);
  });

  it("noindex not allowed", () => {
    assert.deepEqual(checkPage("/x/", page(), { noindex: false }), ["noindex not allowed"]);
  });

  it("two h1", () => {
    assert.deepEqual(checkPage("/x/", page({ body: "<h1>A</h1><h1>B</h1>" }), noindex), [
      "expected one h1, found 2",
    ]);
  });

  it("no h1", () => {
    assert.deepEqual(checkPage("/x/", page({ body: "<p>x</p>" }), noindex), [
      "expected one h1, found 0",
    ]);
  });

  it("h2 then h4 is a skip", () => {
    const body = "<h1>A</h1><h2>B</h2><h4>C</h4>";
    assert.deepEqual(checkPage("/x/", page({ body }), noindex), [
      "heading level skipped: h2 then h4",
    ]);
  });

  it("h3 then h2 is fine", () => {
    const body = "<h1>A</h1><h2>B</h2><h3>C</h3><h2>D</h2>";
    assert.deepEqual(checkPage("/x/", page({ body }), noindex), []);
  });

  it("img without alt", () => {
    const body = '<h1>A</h1><img src="/a.png">';
    assert.deepEqual(checkPage("/x/", page({ body }), noindex), ["image without alt"]);
  });

  it("img with empty alt is fine", () => {
    const body = '<h1>A</h1><img src="/a.png" alt="">';
    assert.deepEqual(checkPage("/x/", page({ body }), noindex), []);
  });

  it("empty link", () => {
    const body = '<h1>A</h1><a href="/x/"></a>';
    assert.deepEqual(
      checkPage("/x/", page({ body }), { noindex: true, distFiles: new Set(["/x/index.html"]) }),
      ["link without a name: /x/"],
    );
  });

  it("link named by aria-label", () => {
    const body = '<h1>A</h1><a href="#main" aria-label="Menu"></a><main id="main"></main>';
    assert.deepEqual(checkPage("/x/", page({ body }), noindex), []);
  });

  it("link named by aria-labelledby that points at text", () => {
    const body = '<h1>A</h1><span id="lbl">Home</span><a href="/x/" aria-labelledby="lbl"></a>';
    assert.deepEqual(
      checkPage("/x/", page({ body }), { noindex: true, distFiles: new Set(["/x/index.html"]) }),
      [],
    );
  });

  it("aria-labelledby that points at an empty element does not name the link", () => {
    const body = '<h1>A</h1><span id="lbl"></span><a href="/x/" aria-labelledby="lbl"></a>';
    assert.deepEqual(
      checkPage("/x/", page({ body }), { noindex: true, distFiles: new Set(["/x/index.html"]) }),
      ["link without a name: /x/"],
    );
  });

  it("button named by an inner svg aria-label", () => {
    const body = '<h1>A</h1><button type="button"><svg aria-label="Close"></svg></button>';
    assert.deepEqual(checkPage("/x/", page({ body }), noindex), []);
  });

  it("link named by an inner svg title", () => {
    const body = '<h1>A</h1><a href="/x/"><svg><title>Home</title></svg></a>';
    assert.deepEqual(
      checkPage("/x/", page({ body }), { noindex: true, distFiles: new Set(["/x/index.html"]) }),
      [],
    );
  });

  it("skip link target #main must exist", () => {
    const body = '<h1>A</h1><a href="#main">Skip to content</a>';
    assert.deepEqual(checkPage("/x/", page({ body }), noindex), ["skip link target #main missing"]);
  });

  it("skip link with its target present", () => {
    const body = '<h1>A</h1><a href="#main">Skip to content</a><main id="main"></main>';
    assert.deepEqual(checkPage("/x/", page({ body }), noindex), []);
  });

  it("link named by an img alt", () => {
    const body = '<h1>A</h1><a href="/x/"><img src="/a.png" alt="Home"></a>';
    assert.deepEqual(
      checkPage("/x/", page({ body }), { noindex: true, distFiles: new Set(["/x/index.html"]) }),
      [],
    );
  });

  it("empty button", () => {
    const body = '<h1>A</h1><button type="button"></button>';
    assert.deepEqual(checkPage("/x/", page({ body }), noindex), ["button without a name"]);
  });

  it("broken internal link", () => {
    const body = '<h1>A</h1><a href="/nope/">Nope</a>';
    assert.deepEqual(checkPage("/x/", page({ body }), noindex), ["broken internal link: /nope/"]);
  });

  it("internal link ok", () => {
    const body = '<h1>A</h1><a href="/docs/">Docs</a><a href="/docs/#x">Docs</a>';
    const distFiles = new Set(["/docs/index.html"]);
    assert.deepEqual(checkPage("/x/", page({ body }), { noindex: true, distFiles }), []);
  });

  it("holding page links skipped before launch", () => {
    const body = '<h1>A</h1><a href="/">Home</a><a href="/#product">Product</a>';
    assert.deepEqual(checkPage("/x/", page({ body }), noindex), []);
  });

  it("external, mailto and in-page links not checked", () => {
    const body =
      '<h1>A</h1><a href="https://example.com/">E</a><a href="//cdn.example.com/x">C</a>' +
      '<a href="mailto:a@b.c">M</a><a href="#main">Main</a><main id="main"></main>';
    assert.deepEqual(checkPage("/x/", page({ body }), noindex), []);
  });

  it("href # not allowed", () => {
    const body = '<h1>A</h1><a href="#">Top</a>';
    assert.deepEqual(checkPage("/x/", page({ body }), noindex), ['href="#" not allowed']);
  });

  it("banned word", () => {
    const body = "<h1>A</h1><p>A powerful tool</p>";
    assert.deepEqual(checkPage("/x/", page({ body }), noindex), ["banned word: powerful"]);
  });

  it("capitalised name", () => {
    const body = "<h1>A</h1><p>Cetra runs</p>";
    assert.deepEqual(checkPage("/x/", page({ body }), noindex), ["write cetra in lower case"]);
  });

  it("banned word in the title", () => {
    assert.deepEqual(checkPage("/x/", page({ title: "<title>Powerful</title>" }), noindex), [
      "banned word: Powerful",
    ]);
  });

  it("capitalised name in the description", () => {
    const description = '<meta name="description" content="Cetra is here">';
    assert.deepEqual(checkPage("/x/", page({ description }), noindex), [
      "write cetra in lower case",
    ]);
  });

  it("banned word in an alt", () => {
    const body = '<h1>A</h1><img src="/a.png" alt="Simply">';
    assert.deepEqual(checkPage("/x/", page({ body }), noindex), ["banned word: Simply"]);
  });

  it("banned word and capitalised name in an aria-label", () => {
    const body = '<h1>A</h1><button type="button" aria-label="Seamless CETRA"></button>';
    assert.deepEqual(checkPage("/x/", page({ body }), noindex), [
      "banned word: Seamless",
      "write cetra in lower case",
    ]);
  });

  it("script and style content ignored", () => {
    const body =
      '<h1>A</h1><script>document.write("<h1>B</h1>")</script>' +
      '<style>p::after { content: "powerful"; }</style>';
    assert.deepEqual(checkPage("/x/", page({ body }), noindex), []);
  });
});

describe("pageWeight", () => {
  it("page weight sums html, css and js", () => {
    const html =
      '<html lang="en"><head><link rel="stylesheet" href="/_astro/b.css"></head>' +
      '<body><script type="module" src="/_astro/a.js"></script><h1>A</h1></body></html>';
    const weight = pageWeight(html, (path) => Buffer.alloc(path.endsWith(".css") ? 100 : 200));
    assert.equal(weight, Buffer.byteLength(html) + 300);
  });
});

describe("pageWeight with asset URLs", () => {
  it("strips the query and fragment, and counts modulepreload links", () => {
    const html =
      '<html lang="en"><head><link rel="stylesheet" href="/_astro/b.css?v=1#x">' +
      '<link rel="modulepreload" href="/_astro/c.js"></head>' +
      '<body><script type="module" src="/_astro/a.js?v=2"></script></body></html>';
    const requested = [];
    const weight = pageWeight(html, (path) => {
      requested.push(path);
      return Buffer.alloc(10);
    });
    assert.deepEqual(requested, ["/_astro/b.css", "/_astro/c.js", "/_astro/a.js"]);
    assert.equal(weight, Buffer.byteLength(html) + 30);
  });
});

describe("listedRoutes", () => {
  const distFiles = new Set([
    "/index.html",
    "/styleguide/index.html",
    "/docs/index.html",
    "/docs/a/index.html",
    "/404.html",
  ]);

  it("lists the built non-docs pages, without the holding page before launch", () => {
    assert.deepEqual(listedRoutes(distFiles, false), ["/styleguide/"]);
  });

  it("includes the home page after launch", () => {
    assert.deepEqual(listedRoutes(distFiles, true), ["/", "/styleguide/"]);
  });
});
