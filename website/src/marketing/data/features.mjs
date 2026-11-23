// The pricing page's feature tables (design §9.9). Each row links to the docs page that backs it
// (`doc`, a path under website/src/content/docs/docs/) or names the decision that sells it
// (`source`, with a `claim` phrase found in that decision). features.test.mjs checks both, and checks the limit rows against pricing.mjs.
import { ENTERPRISE, formatMoney, PLANS, SELF_HOSTED } from "./pricing.mjs";

/**
 * A table cell: `true` is included, `false` is not, a string is a value such as "30 days".
 * @typedef {boolean | string} Cell
 */

/**
 * @typedef {object} FeatureRow
 * @property {string} label
 * @property {string | null} doc docs page, e.g. "tracing/traces"; null only with `source`
 * @property {string} [source] the decision that backs a row with no docs page, e.g. "D177"
 * @property {string} [claim] with `source`: a phrase of that decision's row that backs this row
 * @property {Cell[]} [cloud] Free, Starter, Pro, Enterprise; absent: not in the Cloud table
 * @property {Cell[]} [selfHosted] Self-hosted, licence; absent: not in the Self-hosted table
 */

/** @typedef {{ id: string, title: string, rows: FeatureRow[] }} FeatureGroup */

/** @typedef {{ name: string, price: string }} Column */

/** The value a Pro cell shows for a feature of the Teams add-on. */
export const TEAMS_CELL = "Teams add-on";

/** @type {Column[]} */
export const CLOUD_COLUMNS = [
  ...PLANS.map((plan) => ({ name: plan.name, price: `${formatMoney(plan.priceMonthly)} / month` })),
  { name: ENTERPRISE.name, price: "Custom" },
];

/** @type {Column[]} */
export const SELF_HOSTED_COLUMNS = [
  { name: SELF_HOSTED.name, price: formatMoney(SELF_HOSTED.priceYearly) },
  {
    name: SELF_HOSTED.licence.name,
    price: `${formatMoney(SELF_HOSTED.licence.priceYearly)} / year`,
  },
];

const ALL = [true, true, true, true];
const BOTH = [true, true];
const U = "Unlimited";
const TEAMS = [false, false, TEAMS_CELL, true];
const LICENCE = [false, true];
const ONE_THEN_UNLIMITED = ["1", U, U, U];

/** @type {FeatureGroup[]} */
export const FEATURE_GROUPS = [
  {
    id: "limits",
    title: "Plan limits",
    rows: [
      {
        label: "Spans and scores a month",
        doc: null,
        source: "D177",
        claim: "100k units/month",
        cloud: ["100,000", "200,000", "400,000", "Custom"],
        selfHosted: ["No limit", "No limit"],
      },
      {
        label: "Above the included amount",
        doc: null,
        source: "D177",
        claim: "$4 per 100k",
        cloud: ["Hard limit", "$4 per 100,000", "$4 per 100,000", "Custom"],
      },
      {
        label: "Spend cap",
        doc: null,
        source: "D174",
        claim: "optional spend cap",
        cloud: [false, true, true, true],
      },
      {
        label: "History",
        doc: null,
        source: "D177",
        claim: "30 days data access",
        cloud: ["30 days", "120 days", "5 years", "Custom"],
        selfHosted: ["No limit", "No limit"],
      },
      {
        label: "Seats (viewers are free)",
        doc: null,
        source: "D176",
        claim: "viewers are free and unlimited",
        cloud: ONE_THEN_UNLIMITED,
        selfHosted: [U, U],
      },
      {
        label: "Projects",
        doc: null,
        source: "D-W-21",
        claim: "unlimited projects",
        cloud: ONE_THEN_UNLIMITED,
        selfHosted: [U, U],
      },
      {
        label: "Alert rules",
        doc: "platform/alerts",
        cloud: ["1", "20", U, U],
        selfHosted: [U, U],
      },
      {
        label: "Active online evaluation rules",
        doc: null,
        source: "D178",
        claim: "1 active online-evaluation rule",
        cloud: ["1", "10", U, U],
        selfHosted: [U, U],
      },
      {
        label: "Annotation queues",
        doc: "evaluation/annotation-queues",
        cloud: ["1", "3", U, U],
        selfHosted: [U, U],
      },
      {
        label: "Issue sources",
        doc: "evaluation/issues",
        cloud: ["1", "3", U, U],
        selfHosted: [U, U],
      },
      {
        label: "Ingest rate limit",
        doc: null,
        source: "D177",
        claim: "higher ingest rate limits",
        cloud: ["Standard", "Standard", "Higher", "Custom"],
        selfHosted: ["No limit", "No limit"],
      },
    ],
  },
  {
    id: "tracing",
    title: "Tracing",
    rows: [
      { label: "Trace tree and timeline", doc: "tracing/traces", cloud: ALL, selfHosted: BOTH },
      { label: "Sessions and users", doc: "tracing/sessions", cloud: ALL, selfHosted: BOTH },
      {
        label: "Cost and token tracking",
        doc: "tracing/cost-and-tokens",
        cloud: ALL,
        selfHosted: BOTH,
      },
      {
        label: "Metadata, tags and environments",
        doc: "tracing/metadata-and-tags",
        cloud: ALL,
        selfHosted: BOTH,
      },
      { label: "Images, audio and files", doc: "tracing/media", cloud: ALL, selfHosted: BOTH },
      { label: "Project dashboard", doc: "tracing/dashboard", cloud: ALL, selfHosted: BOTH },
    ],
  },
  {
    id: "search",
    title: "Search, SQL and dashboards",
    rows: [
      {
        label: "Filters, full-text search and saved views",
        doc: "tracing/filters-and-search",
        cloud: ALL,
        selfHosted: BOTH,
      },
      {
        label: "SQL console with saved queries",
        doc: "tracing/sql-console",
        cloud: ALL,
        selfHosted: BOTH,
      },
      {
        label: "Custom dashboards",
        doc: "tracing/custom-dashboards",
        cloud: ALL,
        selfHosted: BOTH,
      },
    ],
  },
  {
    id: "evaluation",
    title: "Evaluation",
    rows: [
      {
        label: "Code checks: regex, JSON schema and more",
        doc: "evaluation/evaluators",
        cloud: ALL,
        selfHosted: BOTH,
      },
      {
        label: "LLM judges on your own model connection",
        doc: "evaluation/evaluators",
        cloud: ALL,
        selfHosted: BOTH,
      },
      { label: "Webhook evaluators", doc: "evaluation/evaluators", cloud: ALL, selfHosted: BOTH },
      {
        label: "Evaluation rules on live traffic",
        doc: "evaluation/online-rules",
        cloud: ALL,
        selfHosted: BOTH,
      },
      {
        label: "Spend caps per judge rule",
        doc: "evaluation/online-rules",
        cloud: ALL,
        selfHosted: BOTH,
      },
      {
        label: "Scores from code, the API or the UI",
        doc: "evaluation/scores",
        cloud: ALL,
        selfHosted: BOTH,
      },
    ],
  },
  {
    id: "datasets",
    title: "Datasets and experiments",
    rows: [
      { label: "Versioned datasets", doc: "evaluation/datasets", cloud: ALL, selfHosted: BOTH },
      {
        label: "Datasets from production traces",
        doc: "evaluation/datasets",
        cloud: ALL,
        selfHosted: BOTH,
      },
      {
        label: "Experiments, compared side by side and run in CI",
        doc: "evaluation/experiments",
        cloud: ALL,
        selfHosted: BOTH,
      },
    ],
  },
  {
    id: "annotation",
    title: "Annotation",
    rows: [
      {
        label: "Manual scoring of traces, spans and sessions",
        doc: "evaluation/annotation",
        cloud: ALL,
        selfHosted: BOTH,
      },
      {
        label: "Annotation queues",
        doc: "evaluation/annotation-queues",
        cloud: ALL,
        selfHosted: BOTH,
      },
    ],
  },
  {
    id: "prompts",
    title: "Prompts",
    rows: [
      {
        label: "Prompt registry with versions and labels",
        doc: "evaluation/prompts",
        cloud: ALL,
        selfHosted: BOTH,
      },
      {
        label: "Prompt fetch with cache and fallback",
        doc: "evaluation/prompts",
        cloud: ALL,
        selfHosted: BOTH,
      },
      {
        label: "Playground with four models side by side",
        doc: "evaluation/playground",
        cloud: ALL,
        selfHosted: BOTH,
      },
    ],
  },
  {
    id: "issues",
    title: "Issues and alerts",
    rows: [
      {
        label: "Errors grouped into issues",
        doc: "evaluation/issues",
        cloud: ALL,
        selfHosted: BOTH,
      },
      {
        label: "Issues from bad scores and user feedback",
        doc: "evaluation/issues",
        cloud: ALL,
        selfHosted: BOTH,
      },
      { label: "Regression detection", doc: "evaluation/issues", cloud: ALL, selfHosted: BOTH },
      {
        label: "Alerts to Slack, email and webhooks",
        doc: "platform/alerts",
        cloud: ALL,
        selfHosted: BOTH,
      },
    ],
  },
  {
    id: "agents",
    title: "Agents and integrations",
    rows: [
      { label: "MCP server for coding agents", doc: "platform/mcp", cloud: ALL, selfHosted: BOTH },
      { label: "In-app AI assistant", doc: "platform/assistant", cloud: ALL, selfHosted: BOTH },
      {
        label: "cetra-cli for setup and queries",
        doc: "reference/cetra-cli",
        cloud: ALL,
        selfHosted: BOTH,
      },
      {
        label: "Tracing for Claude Code, Codex, Gemini CLI and Copilot",
        doc: "integrations",
        cloud: ALL,
        selfHosted: BOTH,
      },
      {
        label: "Python and TypeScript SDKs, framework integrations",
        doc: "integrations",
        cloud: ALL,
        selfHosted: BOTH,
      },
      { label: "OpenTelemetry (OTLP) ingest", doc: "reference/otlp", cloud: ALL, selfHosted: BOTH },
      { label: "REST API", doc: "reference/api/overview", cloud: ALL, selfHosted: BOTH },
    ],
  },
  {
    id: "data",
    title: "Data and privacy",
    rows: [
      {
        label: "Masking and content capture controls",
        doc: "tracing/privacy-controls",
        cloud: ALL,
        selfHosted: BOTH,
      },
      {
        label: "Deletion and GDPR erasure",
        doc: "platform/deletion-and-retention",
        cloud: ALL,
        selfHosted: BOTH,
      },
      {
        label: "Retention controls per project",
        doc: "platform/deletion-and-retention",
        cloud: [false, false, true, true],
        selfHosted: BOTH,
      },
      {
        label: "Usage page by project and day",
        doc: "platform/plans-and-licence",
        cloud: ALL,
        selfHosted: BOTH,
      },
    ],
  },
  {
    id: "deployment",
    title: "Deployment",
    rows: [
      {
        label: "One binary, Docker, Compose or Helm",
        doc: "self-hosting/install",
        selfHosted: BOTH,
      },
      { label: "S3-compatible storage", doc: "self-hosting/s3", selfHosted: BOTH },
      { label: "Online backup and restore", doc: "self-hosting/backup", selfHosted: BOTH },
      {
        label: "Sign-in with GitHub, Google or OpenID Connect",
        doc: "self-hosting/authentication",
        selfHosted: BOTH,
      },
    ],
  },
  {
    id: "access",
    title: "Access and security",
    rows: [
      {
        label: "Organizations, projects and roles",
        doc: "platform/organizations",
        cloud: ALL,
        selfHosted: BOTH,
      },
      { label: "API keys with rotation", doc: "platform/api-keys", cloud: ALL, selfHosted: BOTH },
      {
        label: "Single sign-on per org (OpenID Connect)",
        doc: "platform/sso",
        cloud: TEAMS,
        selfHosted: LICENCE,
      },
      {
        label: "Enforce single sign-on for your domains",
        doc: "platform/sso",
        cloud: TEAMS,
        selfHosted: LICENCE,
      },
      {
        label: "Project-level roles",
        doc: "platform/plans-and-licence",
        cloud: TEAMS,
        selfHosted: LICENCE,
      },
      {
        label: "Audit logs and SCIM",
        doc: null,
        source: "D177",
        claim: "audit logs, SCIM",
        cloud: [false, false, false, true],
      },
      {
        label: "Dedicated cell",
        doc: null,
        source: "D177",
        claim: "dedicated cell",
        cloud: [false, false, false, true],
      },
      {
        label: "Self-hosted licence included",
        doc: null,
        source: "D177",
        claim: "self-hosted licence",
        cloud: [false, false, false, true],
      },
    ],
  },
  {
    id: "support",
    title: "Support",
    rows: [
      { label: "GitHub issues and email", doc: "help/support", cloud: ALL, selfHosted: BOTH },
      {
        label: "Priority support",
        doc: null,
        source: "D177",
        claim: "prioritized support",
        cloud: [false, false, true, true],
      },
      {
        label: "Shared Slack or Teams channel",
        doc: null,
        source: "D177",
        claim: "dedicated Slack or Teams support channel",
        cloud: TEAMS,
      },
      {
        label: "Service level agreements",
        doc: null,
        source: "D177",
        claim: "SLAs",
        cloud: [false, false, false, true],
      },
    ],
  },
];

/**
 * The docs URL of a row's page: "tracing/traces" → "/docs/tracing/traces/".
 * @param {string} doc
 */
export function docHref(doc) {
  return `/docs/${doc}/`;
}

/**
 * The groups of one table, each with only the rows that table shows; a group left empty is
 * dropped.
 * @param {"cloud" | "selfHosted"} table
 * @returns {{ id: string, title: string, rows: { label: string, href: string | null, cells: Cell[] }[] }[]}
 */
export function groupsFor(table) {
  return FEATURE_GROUPS.map((group) => ({
    id: group.id,
    title: group.title,
    rows: group.rows
      .filter((row) => row[table] !== undefined)
      .map((row) => ({
        label: row.label,
        href: row.doc === null ? null : docHref(row.doc),
        cells: /** @type {Cell[]} */ (row[table]),
      })),
  })).filter((group) => group.rows.length > 0);
}
