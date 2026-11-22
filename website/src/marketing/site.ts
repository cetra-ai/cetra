// Every URL and repeated string of the marketing site, in one place (design §3 "Links out").

export const SITE_URL = "https://cetra.sh";
export const GITHUB_REPO = "cetra-ai/cetra";
export const GITHUB_URL = `https://github.com/${GITHUB_REPO}`;
export const LICENSE_URL = `${GITHUB_URL}/blob/main/LICENSE`;
export const DOCS_URL = "/docs/";
export const STATUS_URL = "https://status.cetra.sh";
export const CONTACT_EMAIL = "hello@cetra.sh";

/** Below this the header shows "GitHub" without a number. */
export const MIN_STARS_SHOWN = 50;

/**
 * The cloud. Sources: the control plane's routes `/signup` and `/login` on `app.cetra.sh`
 * (llmobs-control `web/src/routes/`), the status page `status.cetra.sh` (llmobs-control docs).
 */
export const CLOUD = {
  signup: "https://app.cetra.sh/signup",
  login: "https://app.cetra.sh/login",
  /** The regions the cloud offers today (design §9.4; us1 from D-Q-121 in llmobs-control). */
  regions: ["EU (eu1)", "US (us1)"],
} as const;

/**
 * The web demo: the real UI on sample data, with no sign-in and no backend (D-C-22, design §9.6).
 * The custom domain is attached to the `cetra-web-demo` Worker by hand before launch.
 */
export const DEMO_URL = "https://demo.cetra.sh";

/** False until the first release; the layout then shows the release banner. */
export const RELEASED = false;

/** The self-host install commands shown in the copy box (docs: /docs/self-hosting/install/). */
export const INSTALL_COMMANDS = [
  {
    label: "macOS / Linux",
    command:
      "curl -fsSL https://github.com/cetra-ai/cetra/releases/latest/download/install.sh | sh",
  },
  {
    label: "Docker",
    command:
      "docker run -p 8080:8080 -p 4318:4318 -v cetra-data:/data -e CETRA_PUBLIC_URL=http://localhost:8080 ghcr.io/cetra-ai/cetra:latest",
  },
] as const;

export interface NavItem {
  label: string;
  href: string;
  /** Shown only when this marketing route exists (routes.mjs). Absent: always shown. */
  route?: string;
}

export const NAV: NavItem[] = [
  { label: "Product", href: "/#product", route: "/" },
  { label: "Pricing", href: "/pricing/", route: "/pricing/" },
  { label: "Integrations", href: "/integrations/", route: "/integrations/" },
  { label: "Compare", href: "/compare/", route: "/compare/" },
  { label: "Docs", href: DOCS_URL },
];

export interface FooterColumn {
  title: string;
  links: NavItem[];
}

export const FOOTER: FooterColumn[] = [
  {
    title: "Product",
    links: [
      { label: "Pricing", href: "/pricing/", route: "/pricing/" },
      { label: "Self-hosting", href: "/self-hosting/", route: "/self-hosting/" },
      { label: "Security", href: "/security/", route: "/security/" },
      { label: "Integrations", href: "/integrations/", route: "/integrations/" },
      { label: "Compare", href: "/compare/", route: "/compare/" },
    ],
  },
  {
    title: "Developers",
    links: [
      { label: "Docs", href: DOCS_URL },
      { label: "Quick start", href: "/docs/get-started/quickstart/" },
      { label: "Self-hosting guide", href: "/docs/self-hosting/" },
      { label: "API reference", href: "/docs/reference/api/overview/" },
      { label: "GitHub", href: GITHUB_URL },
    ],
  },
  {
    title: "Company",
    links: [
      { label: "Status", href: STATUS_URL },
      { label: "Licence", href: LICENSE_URL },
    ],
  },
];
