import type { PageKind } from "@/lib/assistant/context";

/** What the page is, for the context chip and the composer's placeholder. */
export const PAGE_LABEL: Record<PageKind, string> = {
  trace: "Trace",
  session: "Session",
  user: "User",
  traces: "Traces",
  spans: "Spans",
  sessions: "Sessions",
  users: "Users",
  sql: "SQL",
  dashboard: "Dashboard",
  evaluator: "Evaluator",
  online_rule: "Online rule",
  dataset: "Dataset",
  experiment: "Experiment",
  settings: "Settings",
  other: "Project",
};

/** The composer's placeholder: what the question is about on this page. */
const PLACEHOLDER: Record<PageKind, string> = {
  trace: "Ask about this trace…",
  session: "Ask about this session…",
  user: "Ask about this user…",
  traces: "Ask about these traces…",
  spans: "Ask about these spans…",
  sessions: "Ask about these sessions…",
  users: "Ask about these users…",
  sql: "Ask about this query…",
  dashboard: "Ask about this dashboard…",
  evaluator: "Ask about this evaluator…",
  online_rule: "Ask about this rule…",
  dataset: "Ask about this dataset…",
  experiment: "Ask about this experiment…",
  settings: "Ask about this project…",
  other: "Ask about this project…",
};

export function placeholderOf(page: PageKind): string {
  return PLACEHOLDER[page];
}

const PROJECT_STARTERS = [
  "What happened in this project today?",
  "Which models cost the most this week?",
  "Draft an alert for a high error rate",
];

const LIST_STARTERS = [
  "Summarise the failing traces in this view",
  "What are the slowest traces in this range?",
  "Draft a filter for traces that cost more than $0.10",
];

/** Three questions to start a chat with, chosen for the page (spec 11 § Assistant panel). */
const STARTERS: Record<PageKind, string[]> = {
  trace: [
    "Explain what happened in this trace",
    "Why did this trace fail?",
    "Where is the time spent in this trace?",
  ],
  session: [
    "Summarise this session",
    "Did the user get what they asked for?",
    "Which turn of this session cost the most?",
  ],
  user: [
    "What does this user usually ask about?",
    "How much does this user cost per day?",
    "Did this user hit errors lately?",
  ],
  traces: LIST_STARTERS,
  spans: [
    "Which spans are the slowest in this range?",
    "Which models fail most often?",
    "Draft a filter for LLM spans slower than 5 seconds",
  ],
  sessions: [
    "Which sessions had the most errors?",
    "What do long sessions have in common?",
    "Draft a filter for sessions that cost more than $1",
  ],
  users: [
    "Who are the most expensive users this week?",
    "Which users hit the most errors?",
    "Draft a filter for users with failing traces",
  ],
  sql: [
    "Explain this query",
    "Write a query for the daily cost by model",
    "Why is this query slow?",
  ],
  dashboard: [
    "Why did the error rate change this week?",
    "Which models cost the most?",
    "Draft a chart of p95 latency by model",
  ],
  evaluator: [
    "Explain what this evaluator checks",
    "How have its scores changed lately?",
    "Suggest a stricter version of this evaluator",
  ],
  online_rule: [
    "Explain what this rule samples",
    "How much does this rule cost per day?",
    "Suggest a filter that samples fewer traces",
  ],
  dataset: [
    "Summarise this dataset",
    "Which items look like duplicates?",
    "Draft an evaluator for this dataset",
  ],
  experiment: [
    "Summarise this experiment's results",
    "Which items regressed?",
    "Why did the failing items fail?",
  ],
  settings: PROJECT_STARTERS,
  other: PROJECT_STARTERS,
};

export function startersOf(page: PageKind): string[] {
  return STARTERS[page];
}

/** The context fields' names as the chips show them; an unknown field shows its key. */
const FIELD_LABEL: Record<string, string> = {
  trace_id: "Trace",
  span_id: "Span",
  session_id: "Session",
  user_id: "User",
  filter: "Filter",
  search: "Search",
  range: "Range",
  from: "From",
  to: "To",
  include_eval: "Evaluation traces",
  include_experiments: "Experiment traces",
  view: "View",
  query: "Query",
  params: "Params",
  dashboard_id: "Dashboard",
  evaluator_id: "Evaluator",
  rule_id: "Rule",
  dataset_id: "Dataset",
  experiment_id: "Experiment",
  path: "Path",
};

/** A context field's name for its chip. */
export function fieldLabel(key: string): string {
  return FIELD_LABEL[key] ?? key;
}
