import starlight from "@astrojs/starlight";
import { defineConfig } from "astro/config";
import starlightLinksValidator from "starlight-links-validator";
import starlightSidebarTopics from "starlight-sidebar-topics";

// The site is cetra.sh: the landing page at `/` (src/pages/index.astro) and the
// documentation under `/docs/` (src/content/docs/docs/), D-W-2.
export default defineConfig({
  site: "https://cetra.sh",
  trailingSlash: "always",
  integrations: [
    starlight({
      title: "cetra",
      description:
        "Open-source LLM observability and evaluation that runs as one binary on one machine.",
      favicon: "/favicon.svg",
      customCss: ["./src/styles/custom.css"],
      social: [{ icon: "github", label: "GitHub", href: "https://github.com/cetra-ai/cetra" }],
      editLink: { baseUrl: "https://github.com/cetra-ai/cetra/edit/main/website/" },
      // CI checks out one commit; git dates would all be the same.
      lastUpdated: false,
      plugins: [
        starlightLinksValidator(),
        starlightSidebarTopics([
          {
            label: "Docs",
            link: "/docs/",
            icon: "open-book",
            items: [{ label: "Overview", link: "/docs/" }],
          },
          {
            label: "Reference",
            link: "/docs/reference/cli/",
            icon: "document",
            items: [{ label: "CLI", link: "/docs/reference/cli/" }],
          },
        ]),
      ],
    }),
  ],
});
