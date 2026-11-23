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

/** The calculator's starting values: a typical team's monthly spans and scores. */
export const DEFAULT_SPANS = 1_000_000;
export const DEFAULT_SCORES = 50_000;

/** The largest unit count the calculator takes. At or above it the estimate is "Talk to us". */
export const MAX_UNITS = 1_000_000_000_000;

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
    rules: "1 alert rule, 1 online evaluation rule, 1 annotation queue and 1 issue source",
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
    rules: "20 alert rules, 10 online evaluation rules, 3 annotation queues and 3 issue sources",
    support: null,
    cta: "Start free",
    blurb: "For a team shipping its first LLM features.",
  },
  {
    id: "pro",
    name: "Pro",
    priceMonthly: 150,
    includedUnits: 400_000,
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
    "A self-hosted licence",
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
    /** A service, not a gated feature (D197). */
    support: "Email support from the cetra team, with a reply within 2 business days",
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
  const free = planById("free");
  return units <= free.includedUnits ? free : planById("starter");
}

/**
 * @param {string} id
 * @returns {Plan}
 */
function planById(id) {
  const plan = PLANS.find((p) => p.id === id);
  if (!plan) throw new Error(`no plan with id ${id}`);
  return plan;
}

/**
 * The estimate shown for one plan: a price, or the reason there is none.
 * @param {Plan} plan
 * @param {number} units
 */
export function estimateText(plan, units) {
  if (units > MAX_UNITS - 1) return "Talk to us";
  const price = monthlyPrice(plan, units);
  if (price === null) return `Not available above ${formatUnits(plan.includedUnits)} a month`;
  return formatMoney(price);
}

/**
 * A list price in USD, rounded up to the dollar: 30.4 → "$31", 1184 → "$1,184".
 * @param {number} usd
 */
export function formatMoney(usd) {
  return `$${Math.ceil(usd).toLocaleString("en-US")}`;
}

/**
 * A unit count typed into the calculator: a whole number from 0 to MAX_UNITS. Empty, negative
 * and not-a-number input counts as 0; a value above MAX_UNITS (including Infinity) is MAX_UNITS.
 * @param {string | number} raw
 */
export function parseUnits(raw) {
  const n = Math.floor(Number(raw));
  if (Number.isNaN(n) || n < 0) return 0;
  return Math.min(n, MAX_UNITS);
}

/**
 * A unit count for display, with thousands separators: 100000 → "100,000".
 * @param {number} n
 */
export function formatUnits(n) {
  return n.toLocaleString("en-US");
}
