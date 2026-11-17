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
            label: "Self-hosting",
            link: "/docs/self-hosting/",
            icon: "setting",
            items: [
              { label: "Overview", link: "/docs/self-hosting/" },
              {
                label: "Install",
                items: [
                  { label: "Install", link: "/docs/self-hosting/install/" },
                  { label: "Docker Compose", link: "/docs/self-hosting/docker-compose/" },
                  { label: "Kubernetes", link: "/docs/self-hosting/kubernetes/" },
                ],
              },
              {
                label: "Configure",
                items: [
                  { label: "Configuration", link: "/docs/self-hosting/configuration/" },
                  {
                    label: "Configuration reference",
                    link: "/docs/self-hosting/configuration-reference/",
                  },
                  { label: "Object storage (S3)", link: "/docs/self-hosting/s3/" },
                  { label: "Security", link: "/docs/self-hosting/security/" },
                  { label: "Resources", link: "/docs/self-hosting/resources/" },
                ],
              },
              {
                label: "Operate",
                items: [
                  { label: "Backup and restore", link: "/docs/self-hosting/backup/" },
                  { label: "Monitoring", link: "/docs/self-hosting/monitoring/" },
                  { label: "Upgrade", link: "/docs/self-hosting/upgrade/" },
                ],
              },
            ],
          },
        ]),
      ],
    }),
  ],
});
