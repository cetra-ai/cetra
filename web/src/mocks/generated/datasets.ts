import type { Schema } from "@/api/types";
import { histogramOf } from "@/mocks/generated/experiments";
import { mulberry32 } from "@/mocks/generated/random";

type DatasetRow = Schema<"Dataset">;
type ItemRow = Schema<"DatasetItem">;
type VersionRow = Schema<"DatasetVersion">;
type ExperimentRow = Schema<"Experiment">;
type ExperimentStatus = Schema<"ExperimentStatus">;
type SummaryRow = Schema<"ExperimentSummary">;
type ResultRowData = Schema<"ResultRow">;
type TraceRow = Schema<"TraceSummary">;

export type GeneratedExperiment = {
  experiment: ExperimentRow;
  summary: SummaryRow;
  /** One row per item of the pinned version and repetition, in item then repetition order. */
  rows: ResultRowData[];
};

/** One change of a version to one item (D-J-24). `before` is the revision it replaced or deleted. */
export type GeneratedChange = {
  version: number;
  item_id: string;
  change: Schema<"ItemChangeKind">;
  before?: ItemRow;
  after?: ItemRow;
};

export type GeneratedDataset = {
  dataset: DatasetRow;
  /** The live revisions, in id order, which is the order the server lists them in. */
  items: ItemRow[];
  /** Every change, oldest version first. `itemsAt` reads the items of a version from it. */
  history: GeneratedChange[];
  /** Oldest first. The handlers list them newest first. */
  versions: VersionRow[];
  /** Oldest first. */
  experiments: GeneratedExperiment[];
};

const DAY_MS = 86_400_000;
const HOUR_MS = 3_600_000;
/** The member user the versions and items are attributed to (the same id the fixtures use). */
const MEMBER_ID = "01900000-0000-7000-8000-000000000100";

const NAMES = [
  "faq",
  "refund-requests",
  "billing-edge-cases",
  "tone-checks",
  "password-resets",
  "api-rate-limits",
  "webhook-setup",
  "plan-changes",
  "ticket-summaries",
  "escalation-rules",
  "sentiment-labels",
  "intent-routing",
  "pii-redaction",
  "citation-check",
  "code-snippets",
  "onboarding-questions",
  "export-formats",
  "outage-status",
  "duplicate-tickets",
  "language-detection",
  "policy-lookup",
  "persona-voice",
  "long-threads",
  "empty-drafts",
] as const;

/** One sentence for each dataset that has a description; the rest have none (see `hasDescription`). */
const DESCRIPTIONS = [
  "Common customer questions with the answers support gives.",
  "Refund requests across plans, with the expected decision.",
  "Billing cases that are easy to get wrong.",
  "Replies checked for a calm, direct tone.",
  "Password reset questions from signed-out users.",
  "Questions about the API rate limit and retries.",
  "Webhook setup and delivery problems.",
  "Plan and price change questions.",
  "Ticket summaries written for the support queue.",
  "Cases that must be escalated to a human agent.",
  "Customer messages labelled with their sentiment.",
  "Messages routed to the right team.",
  "Text with personal data that must be redacted.",
  "Answers that must cite the help center.",
  "Code questions with a snippet in the answer.",
  "First-week questions from new accounts.",
  "Questions about export formats and limits.",
  "Questions about outages and status updates.",
  "Pairs of tickets that describe the same problem.",
  "Messages in several languages.",
  "Policy questions answered from the handbook.",
  "Replies written in the brand voice.",
  "Long support threads, one question each.",
  "Drafts that are not written yet.",
] as const;

/** The datasets whose input is `{question}` and that have an `input_schema` (index 0 and 4). */
const SCHEMA_DATASETS = new Set([0, 4]);
/** The one dataset whose last experiment failed. */
const FAILED_DATASET = 5;
/** The datasets whose last experiment is still running. */
const RUNNING_DATASETS = new Set([2, 9, 16]);

const QUESTIONS = [
  "How do I get a refund for a duplicate charge?",
  "I forgot my password. How do I reset it?",
  "Why am I getting 429 errors from the API?",
  "How can I export my traces to a CSV file?",
  "How do I set up a webhook for new tickets?",
  "Why was my plan price changed this month?",
  "Can I change my plan in the middle of a billing cycle?",
  "Does the refund go back to the same card?",
  "Where can I download past invoices?",
  "Do all API keys share the same rate limit?",
  "My webhook deliveries keep failing. What should I check?",
  "Can an agent see the customer's order history?",
] as const;

/** The expected answer to `QUESTIONS[i]`, at the same index. */
const ANSWERS = [
  "Refund the duplicate invoice from the billing page within 14 days.",
  "Use the reset link on the sign-in page. It expires after 30 minutes.",
  "Read the Retry-After header and back off before you retry.",
  "Export from the Traces page, or call the export endpoint for large ranges.",
  "Register an HTTPS endpoint and subscribe it to ticket.created.",
  "Plan changes apply from the next billing date.",
  "Yes. The change takes effect on the next invoice.",
  "Yes. Refunds return to the card that paid the invoice.",
  "Every invoice is listed under Billing, with the price used for each cycle.",
  "No. The limit applies to the whole project.",
  "Check that the endpoint is public and returns a success status.",
  "Yes, within the support scope of the ticket.",
] as const;

const SUBJECTS = ["Duplicate charge", "Locked account", "Rate limit", "Export", "Webhook"] as const;
const PLANS = ["free", "pro", "team"] as const;
const CODES = ["invoice_id", "retry_after", "ticket.created"] as const;
const TOPICS = ["billing", "refunds", "auth", "api", "webhooks", "tone"] as const;
const DIFFICULTIES = ["easy", "hard"] as const;
const EXPERIMENT_NAMES = [
  "gpt-4.1 baseline",
  "prompt v3",
  "claude-sonnet-4-5 rerun",
  "prompt v4 with examples",
  "gpt-4.1-mini",
  "gemini-2.5-pro",
  "retrieval top-8",
  "temperature 0",
] as const;
const FAILURE = "The judge endpoint timed out after three retries.";
const ROW_ERROR = "The task timed out.";
const FALLBACK_OUTPUT = "I could not find this in the knowledge base.";

/** One revision of an item. `ends` is the version that replaced or deleted it; absent while it is live. */
type BuiltItem = { row: ItemRow; answer: string; ends?: number };
type BuiltDataset = {
  row: DatasetRow;
  /** Every revision of every item. */
  items: BuiltItem[];
  history: GeneratedChange[];
  versions: VersionRow[];
};
type SourceFields = Partial<Pick<ItemRow, "source_trace_id" | "source_span_id">>;
type PendingExperiment = {
  datasetIndex: number;
  /** Position among this dataset's experiments, oldest first. */
  ordinal: number;
  experimentCount: number;
  pinned: BuiltItem[];
  datasetVersion: number;
  createdMs: number;
  finishedMs: number | undefined;
  repetitions: number;
  name: string;
  status: ExperimentStatus;
  resultCount: number;
};

/** A UUID in the shape of the other generated ids. `kind` separates the kinds of row; `index` numbers the row. */
function uuid(kind: number, index: number): string {
  return `00000000-0000-7000-8000-${kind.toString(16)}${index.toString(16).padStart(11, "0")}`;
}

function rfc3339(ms: number): string {
  return new Date(ms)
    .toISOString()
    .replace(/\.(\d{3})Z$/, (_match, fraction: string) => `.${fraction}000000Z`);
}

/** A random source for one stream of one dataset or experiment: the same inputs give the same numbers. */
function streamRandom(seed: number, stream: number, index: number): () => number {
  return mulberry32(
    Math.imul(seed, 0x9e3779b1) ^ Math.imul(stream * 100_003 + index + 1, 0x85ebca6b),
  );
}

function pick<T>(items: readonly T[], rng: () => number): T {
  const value = items[Math.floor(rng() * items.length)];
  if (value === undefined) {
    throw new Error("pick from an empty list");
  }
  return value;
}

/** The version's time, spread evenly between the dataset's first change and `now`. */
function versionTime(start: number, span: number, current: number, version: number): number {
  return start + Math.floor((span * version) / (current + 1));
}

/**
 * The input of an item: one of four shapes, chosen by the dataset index. `shape` 0 is a question
 * object, 1 a chat array, 2 a plain string and 3 a nested ticket. Some text is multi-line Markdown.
 */
function inputOf(shape: number, question: string, rng: () => number): unknown {
  const code = pick(CODES, rng);
  const text =
    rng() < 0.3 ? `## Question\n\n${question}\n\n- Check the \`${code}\` field first` : question;
  switch (shape) {
    case 0:
      return { question: text };
    case 1:
      return {
        messages: [
          { role: "system", content: "You are a concise support agent." },
          { role: "user", content: text },
        ],
      };
    case 2:
      return text;
    default:
      return {
        ticket: { subject: pick(SUBJECTS, rng), body: text },
        customer: { plan: pick(PLANS, rng) },
      };
  }
}

/** The expected output of an item: absent for about one item in five, else a string or `{answer}`. */
function expectedOf(answer: string, rng: () => number): unknown | undefined {
  if (rng() >= 0.8) {
    return undefined;
  }
  return rng() < 0.5 ? answer : { answer };
}

/** `text` wrapped in the same form as `expected`: a bare string, or `{answer}`. */
function wrapLike(expected: unknown, text: string): unknown {
  return typeof expected === "object" && expected !== null ? { answer: text } : text;
}

/** The answer's variation: the same answer, or the same answer with a short note. */
function variationOf(answer: string): string {
  return answer.endsWith(".") ? `${answer} Please check the policy page.` : answer;
}

/** The forms that add items: 0 (add), 3 (import) and 4 (add or add from a trace). Forms 1 and 2 edit or delete. */
const ADD_FORMS = new Set([0, 3, 4]);

/** The copied trace of about one item in three, as the source fields; `{}` for none. */
function randomSource(traces: readonly TraceRow[], rng: () => number): SourceFields {
  const trace = traces.length > 0 && rng() < 0.3 ? pick(traces, rng) : undefined;
  return trace?.root_span_id
    ? { source_trace_id: trace.trace_id, source_span_id: trace.root_span_id }
    : {};
}

/** An edited item keeps the source of the revision it replaces (D-J-9). */
function inheritedSource(row: ItemRow): SourceFields {
  return row.source_trace_id !== undefined && row.source_span_id !== undefined
    ? { source_trace_id: row.source_trace_id, source_span_id: row.source_span_id }
    : {};
}

/** Whether a revision is part of the dataset at `version`: started at or before it, and not yet replaced. */
function presentAt(item: BuiltItem, version: number): boolean {
  return item.row.version <= version && (item.ends === undefined || item.ends > version);
}

/**
 * The items of a version (D-J-24): each item's revision at that version, in id order. An item
 * deleted at or before the version is left out; an edited item shows its replacement from its edit on.
 */
export function itemsAt(generated: GeneratedDataset, version: number): ItemRow[] {
  const latest = new Map<string, GeneratedChange>();
  for (const change of generated.history) {
    if (change.version <= version) {
      latest.set(change.item_id, change);
    }
  }
  const rows: ItemRow[] = [];
  for (const change of latest.values()) {
    if (change.change !== "deleted" && change.after !== undefined) {
      rows.push(change.after);
    }
  }
  return rows.sort((left, right) => (left.id < right.id ? -1 : left.id > right.id ? 1 : 0));
}

export function createDatasets(options: {
  seed: number;
  now: number;
  traces: readonly TraceRow[];
}): GeneratedDataset[] {
  const { seed, now, traces } = options;
  let itemSerial = 0;
  let revisionSerial = 0;
  const built: BuiltDataset[] = [];
  const pending: PendingExperiment[] = [];

  /** One revision of an item, its content drawn from `rng`. */
  function revisionOf(args: {
    rng: () => number;
    shape: number;
    itemId: string;
    version: number;
    at: number;
    source: SourceFields;
  }): BuiltItem {
    const { rng, shape, itemId, version, at, source } = args;
    revisionSerial += 1;
    const questionIndex = Math.floor(rng() * QUESTIONS.length);
    const question = QUESTIONS[questionIndex] ?? QUESTIONS[0];
    const answer = ANSWERS[questionIndex] ?? ANSWERS[0];
    const input = inputOf(shape, question, rng);
    const expected = expectedOf(answer, rng);
    const metadata =
      rng() < 0.5 ? {} : { topic: pick(TOPICS, rng), difficulty: pick(DIFFICULTIES, rng) };
    return {
      answer,
      row: {
        id: itemId,
        revision_id: uuid(3, revisionSerial),
        version,
        created_at: rfc3339(at),
        input,
        metadata,
        ...(expected === undefined ? {} : { expected_output: expected }),
        ...source,
      },
    };
  }

  NAMES.forEach((name, datasetIndex) => {
    const rng = streamRandom(seed, 1, datasetIndex);
    const empty = name === "empty-drafts";
    const createdMs = now - Math.floor((10 + rng() * 110) * DAY_MS);
    const start = createdMs + HOUR_MS;
    const span = now - start;
    const current = empty ? 0 : 1 + Math.floor(rng() * 30);
    const shape = datasetIndex % 4;
    const addVersions: number[] = [];
    for (let version = 1; version <= current; version += 1) {
      if (ADD_FORMS.has((version - 1) % 5)) {
        addVersions.push(version);
      }
    }
    // Every add version introduces at least one item; edit and delete versions introduce none.
    const itemCount = empty ? 0 : Math.max(addVersions.length, 5 + Math.floor(rng() * 396));
    const introducing = [...addVersions];
    while (introducing.length < itemCount) {
      introducing.push(pick(addVersions, rng));
    }
    const items: BuiltItem[] = [];
    for (const version of introducing) {
      itemSerial += 1;
      items.push(
        revisionOf({
          rng,
          shape,
          itemId: uuid(2, itemSerial),
          version,
          at: versionTime(start, span, current, version),
          source: randomSource(traces, rng),
        }),
      );
    }

    const history: GeneratedChange[] = [];
    const versions: VersionRow[] = [];
    for (let version = 1; version <= current; version += 1) {
      const form = (version - 1) % 5;
      const when = versionTime(start, span, current, version);
      const eventRng = streamRandom(seed, 10, datasetIndex * 100 + version);
      let description: string;
      let counts = { added: 0, edited: 0, deleted: 0 };
      if (form === 1 || form === 2) {
        const candidates = items.filter(
          (item) => item.ends === undefined && item.row.version < version,
        );
        const target = candidates[Math.floor(eventRng() * candidates.length)];
        if (!target) {
          throw new Error(`${name} has no live item to change at version ${version}`);
        }
        target.ends = version;
        if (form === 1) {
          const next = revisionOf({
            rng: eventRng,
            shape,
            itemId: target.row.id,
            version,
            at: when,
            source: inheritedSource(target.row),
          });
          items.push(next);
          history.push({
            version,
            item_id: target.row.id,
            change: "edited",
            before: target.row,
            after: next.row,
          });
          counts = { added: 0, edited: 1, deleted: 0 };
          description = `edit item ${target.row.id}`;
        } else {
          history.push({ version, item_id: target.row.id, change: "deleted", before: target.row });
          counts = { added: 0, edited: 0, deleted: 1 };
          description = `delete item ${target.row.id}`;
        }
      } else {
        const introduced = items.filter((item) => item.row.version === version);
        for (const item of introduced) {
          history.push({ version, item_id: item.row.id, change: "added", after: item.row });
        }
        const count = introduced.length;
        counts = { added: count, edited: 0, deleted: 0 };
        const fromTrace =
          traces.length > 0 ? pick(traces, streamRandom(seed, 9, version)) : undefined;
        if (form === 0) {
          description = `add ${count} items`;
        } else if (form === 3) {
          description = `import faq.csv (${count} items)`;
        } else {
          description =
            fromTrace && count === 1
              ? `add from trace ${fromTrace.trace_id}`
              : `add ${count} items`;
        }
      }
      const createdBy = streamRandom(seed, 4, datasetIndex * 100 + version);
      const roll = createdBy();
      versions.push({
        version,
        description,
        ...counts,
        created_at: rfc3339(when),
        ...(roll < 0.6
          ? { created_by: MEMBER_ID }
          : roll < 0.8
            ? { created_by: `key:${uuid(5, datasetIndex * 100 + version)}` }
            : {}),
      });
    }
    items.sort((left, right) =>
      left.row.id < right.row.id
        ? -1
        : left.row.id > right.row.id
          ? 1
          : left.row.version - right.row.version,
    );
    const live = items.filter((item) => item.ends === undefined);

    const experimentRng = streamRandom(seed, 2, datasetIndex);
    const experimentCount = itemCount > 0 ? Math.floor(experimentRng() * 13) : 0;
    for (let ordinal = 0; ordinal < experimentCount; ordinal += 1) {
      const datasetVersion = 1 + Math.floor(experimentRng() * current);
      const pinned = items.filter((item) => presentAt(item, datasetVersion));
      const versionAt = versionTime(start, span, current, datasetVersion);
      const experimentMs = versionAt + Math.floor(experimentRng() * (now - versionAt) * 0.9);
      const repetitions = 1 + Math.floor(experimentRng() * 3);
      const last = ordinal === experimentCount - 1;
      const status: ExperimentStatus =
        last && datasetIndex === FAILED_DATASET
          ? "failed"
          : last && RUNNING_DATASETS.has(datasetIndex)
            ? "running"
            : "completed";
      const total = pinned.length * repetitions;
      const resultCount =
        status === "completed"
          ? total
          : status === "failed"
            ? Math.floor(total * 0.4)
            : Math.floor(total * 0.6);
      pending.push({
        datasetIndex,
        ordinal,
        experimentCount,
        pinned,
        datasetVersion,
        createdMs: experimentMs,
        finishedMs:
          status === "running"
            ? undefined
            : Math.min(now - 1, experimentMs + (5 + Math.floor(experimentRng() * 55)) * 60_000),
        repetitions,
        name: EXPERIMENT_NAMES[(datasetIndex * 3 + ordinal) % EXPERIMENT_NAMES.length] ?? "",
        status,
        resultCount,
      });
    }

    built.push({
      row: {
        id: uuid(1, datasetIndex + 1),
        name,
        ...(datasetIndex % 3 === 2 ? {} : { description: DESCRIPTIONS[datasetIndex] ?? "" }),
        ...(SCHEMA_DATASETS.has(datasetIndex)
          ? { input_schema: { type: "object", required: ["question"] } }
          : {}),
        created_at: rfc3339(createdMs),
        updated_at: rfc3339(current > 0 ? versionTime(start, span, current, current) : start),
        current_version: current,
        item_count: live.length,
        experiment_count: experimentCount,
      },
      items,
      history,
      versions,
    });
  });

  // Experiment ids follow creation time, as the server's UUIDv7 ids do.
  pending.sort(
    (left, right) =>
      left.createdMs - right.createdMs ||
      left.datasetIndex - right.datasetIndex ||
      left.ordinal - right.ordinal,
  );
  const experiments = pending.map((experiment, index) =>
    buildExperiment(experiment, index, seed, traces, built),
  );

  return built.map((dataset, datasetIndex) => ({
    dataset: dataset.row,
    items: dataset.items.filter((item) => item.ends === undefined).map((item) => item.row),
    history: dataset.history,
    versions: dataset.versions,
    experiments: experiments
      .filter((item) => item.datasetIndex === datasetIndex)
      .map((item) => item.generated),
  }));
}

function buildExperiment(
  pending: PendingExperiment,
  index: number,
  seed: number,
  traces: readonly TraceRow[],
  built: readonly BuiltDataset[],
): { datasetIndex: number; generated: GeneratedExperiment } {
  const dataset = built[pending.datasetIndex]?.row;
  if (!dataset) {
    throw new Error("experiment of an unknown dataset");
  }
  const rng = streamRandom(seed, 3, index + 1);
  const id = uuid(4, index + 1);
  const experiment: ExperimentRow = {
    id,
    dataset_id: dataset.id,
    dataset_name: dataset.name,
    dataset_version: pending.datasetVersion,
    created_at: rfc3339(pending.createdMs),
    item_count: pending.pinned.length,
    metadata: {},
    name: pending.name,
    repetitions: pending.repetitions,
    result_count: pending.resultCount,
    status: pending.status,
    ...(pending.finishedMs === undefined ? {} : { finished_at: rfc3339(pending.finishedMs) }),
    ...(pending.status === "failed" ? { error: FAILURE } : {}),
  };

  // Correctness drifts up across a dataset's experiments; relevance passes more often as well.
  const progress = pending.ordinal / Math.max(1, pending.experimentCount - 1);
  const rows: ResultRowData[] = [];
  const total = pending.pinned.length * pending.repetitions;
  for (let slot = 0; slot < Math.min(total, pending.resultCount); slot += 1) {
    const item = pending.pinned[Math.floor(slot / pending.repetitions)];
    if (!item) {
      break;
    }
    const repetition = slot % pending.repetitions;
    const failed = rng() < 0.04;
    const expected = item.row.expected_output;
    const output =
      expected === undefined
        ? FALLBACK_OUTPUT
        : wrapLike(expected, rng() < 0.7 ? item.answer : variationOf(item.answer));
    const correctness = Math.min(
      1,
      Math.max(0, Math.round((0.55 + 0.3 * progress + (rng() - 0.5) * 0.16) * 100) / 100),
    );
    const relevant = rng() < 0.7 + 0.2 * progress ? 1 : 0;
    const trace = traces.length > 0 ? pick(traces, rng) : undefined;
    rows.push({
      dataset_item_id: item.row.id,
      repetition,
      input: item.row.input,
      ...(expected === undefined ? {} : { expected_output: expected }),
      ...(failed ? {} : { output }),
      latency_ns: Math.floor((200 + rng() * 8_000) * 1_000_000),
      cost_total: Math.round(rng() * 4_000) / 1_000_000,
      ...(failed ? { error: ROW_ERROR } : {}),
      ...(trace ? { trace_id: trace.trace_id } : {}),
      scores: failed
        ? {}
        : {
            correctness: { data_type: "numeric", value_num: correctness },
            relevance: { data_type: "boolean", value_num: relevant },
          },
    });
  }

  return {
    datasetIndex: pending.datasetIndex,
    generated: { experiment, summary: summaryOf(experiment, rows), rows },
  };
}

function summaryOf(experiment: ExperimentRow, rows: readonly ResultRowData[]): SummaryRow {
  const latencies = rows
    .map((row) => row.latency_ns)
    .filter((value): value is number => value !== undefined)
    .sort((left, right) => left - right);
  const costs = rows.map((row) => row.cost_total ?? 0);
  const correctness = rows
    .map((row) => row.scores.correctness?.value_num)
    .filter((value): value is number => value !== undefined);
  const relevance = rows
    .map((row) => row.scores.relevance?.value_num)
    .filter((value): value is number => value !== undefined);
  const scores: SummaryRow["scores"] = [];
  if (correctness.length > 0) {
    const sorted = [...correctness].sort((left, right) => left - right);
    scores.push({
      name: "correctness",
      data_type: "numeric",
      count: correctness.length,
      labels: {},
      mean: round(sum(correctness) / correctness.length),
      median: sorted[Math.ceil(sorted.length / 2) - 1],
      min: sorted[0],
      max: sorted[sorted.length - 1],
      histogram: histogramOf("numeric", sorted),
    });
  }
  if (relevance.length > 0) {
    scores.push({
      name: "relevance",
      data_type: "boolean",
      count: relevance.length,
      labels: {},
      pass_rate: round(sum(relevance) / relevance.length),
      histogram: [],
    });
  }
  return {
    experiment,
    error_count: rows.filter((row) => row.error !== undefined).length,
    result_count: rows.length,
    ...(latencies.length === 0
      ? {}
      : {
          latency_mean_ns: Math.round(sum(latencies) / latencies.length),
          latency_p50_ns: nearestRank(latencies, 0.5),
          latency_p95_ns: nearestRank(latencies, 0.95),
        }),
    ...(rows.length === 0 ? {} : { cost_total: round(sum(costs), 6) }),
    scores,
  };
}

function nearestRank(sorted: readonly number[], fraction: number): number {
  return sorted[Math.max(0, Math.ceil(sorted.length * fraction) - 1)] ?? 0;
}

function sum(values: readonly number[]): number {
  return values.reduce((total, value) => total + value, 0);
}

function round(value: number, digits = 2): number {
  const factor = 10 ** digits;
  return Math.round(value * factor) / factor;
}
