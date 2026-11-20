// Cloud plans and the self-hosted licence (D172–D178, D-Q-82). Every price on the site comes
// from here; pricing.test.mjs checks the numbers against D177 in docs/mvp-decisions.md and the
// licence features against website/src/content/docs/docs/platform/plans-and-licence.mdx.
// `projects` and `support` are null where D177 states nothing.

/**
 * @typedef {object} Plan
 * @property {string} id
 * @property {string} name
 * @property {number} priceMonthly USD
 * @property {number} includedUnits spans + scores a month
 * @property {boolean} overage
 * @property {string} dataAccess
 * @property {string} seats
 * @property {string | null} projects
 * @property {string} rules
 * @property {string | null} support
 * @property {string} cta the button label; the button goes to CLOUD.signup
 * @property {string} blurb
 */

export const UNIT = "span or score";

/** USD per 100,000 units over the included amount (D177: "$4 per 100k, lower with volume"). */
export const OVERAGE_PER_100K = 4;

/** @type {Plan[]} */
export const PLANS = [
  {
    id: "free",
    name: "Free",
    priceMonthly: 0,
    includedUnits: 100_000,
    overage: false,
    dataAccess: "30 days",
    seats: "1 seat",
    projects: "1 project",
    rules: "1 alert rule and 1 online evaluation rule",
    support: null,
    cta: "Start free",
    blurb: "For trying cetra on a side project.",
  },
  {
    id: "starter",
    name: "Starter",
    priceMonthly: 30,
    includedUnits: 200_000,
    overage: true,
    dataAccess: "120 days",
    seats: "Unlimited seats",
    projects: null,
    rules: "Unlimited rules",
    support: null,
    cta: "Start free",
    blurb: "For a team shipping its first LLM features.",
  },
  {
    id: "pro",
    name: "Pro",
    priceMonthly: 150,
    includedUnits: 200_000,
    overage: true,
    dataAccess: "5 years",
    seats: "Unlimited seats",
    projects: null,
    rules: "Unlimited rules",
    support: "Priority support",
    cta: "Start free",
    blurb: "For production apps: long history, retention controls, higher ingest limits.",
  },
];

export const TEAMS_ADDON = {
  name: "Teams add-on",
  priceMonthly: 300,
  requires: "Pro",
  features: [
    "Single sign-on through your OpenID Connect provider, such as Okta",
    "Enforce single sign-on for your domains",
    "Project-level roles",
    "A shared Slack or Teams channel with us",
  ],
};

export const ENTERPRISE = {
  name: "Enterprise",
  features: [
    "A dedicated cell",
    "Custom licence terms for self-hosting",
    "Audit logs and SCIM",
    "Service level agreements",
  ],
};

export const SELF_HOSTED = {
  name: "Self-hosted",
  priceYearly: 0,
  features: [
    "Every core feature, with no limits on volume, seats or retention",
    "One binary, Docker, docker-compose or Helm",
    "Your data never leaves your infrastructure",
  ],
  licence: {
    name: "Self-hosted licence",
    priceYearly: 5_400,
    features: [
      "Single sign-on per org, with verified domains",
      "Enforce single sign-on for your domains",
      "Project-level roles",
    ],
  },
};

/**
 * Monthly list price of a paid plan for a number of units (no volume discount, no tax).
 * @param {{ priceMonthly: number, includedUnits: number, overage: boolean }} plan
 * @param {number} units
 * @returns {number | null} null when the plan has no overage and the units exceed it
 */
export function monthlyPrice(plan, units) {
  const extra = Math.max(0, units - plan.includedUnits);
  if (extra > 0 && !plan.overage) return null;
  return plan.priceMonthly + (extra / 100_000) * OVERAGE_PER_100K;
}

/**
 * The cheapest plan for a number of units: Free when it fits, else Starter.
 * @param {number} units
 */
export function suggestedPlan(units) {
  return units <= PLANS[0].includedUnits ? PLANS[0] : PLANS[1];
}

/**
 * A unit count for display, with thousands separators: 100000 → "100,000".
 * @param {number} n
 */
export function formatUnits(n) {
  return n.toLocaleString("en-US");
}
