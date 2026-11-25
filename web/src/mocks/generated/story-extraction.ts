import type { Schema } from "@/api/types";
import {
  type Dataset,
  evenSpread,
  isEligibleTrace,
  type Mark,
  type MarkedSpan,
  type MarksContext,
  mergeMarks,
  type Profile,
  type Turn,
} from "@/mocks/generated/dataset";
import { EXTRACTION_PROFILE } from "@/mocks/generated/profiles";

/** The extraction project's story (M17): comma-decimal vendors, re-extractions, scanned PDFs and a failing batch. */

const HOUR_MS = 3_600_000;
const DAY_MS = 86_400_000;
const BATCH_TAG = "batch-1119";
const BATCH_SIZE = 200;
/** Batch ranks start at 0: rank r starts at `B + r * BATCH_STEP_MS`. */
const BATCH_STEP_MS = 18_000;
/** The batch's newest three ranks fail twice, so the newest-first list shows them first (M19). */
const BATCH_FAILURE_RANKS: ReadonlySet<number> = new Set([197, 198, 199]);
const BURST_SIZE = 12;

/** The four comma-decimal vendors (M17). Each is a turn that only marked traces carry. */
const COMMA_VENDORS: readonly {
  turn: Turn;
  /** The amount as the summary shows it, quoted in the failed check's reason. */
  amount: string;
  /** The share of the vendor's eligible traces that are re-extracted. */
  share: number;
}[] = [
  {
    turn: {
      question: "invoice-2026-0521.pdf (2 pages)",
      heading: "Nordic Freight",
      summary: "Nordic Freight · INV-2026-0521 · 2026-11-18 · 6 line items · 1.234,56 EUR",
      steps: [],
      followUps: EXTRACTION_PROFILE.turns[0].followUps,
    },
    amount: "1.234,56 EUR",
    share: 0.38,
  },
  {
    turn: {
      question: "invoice-2026-0524.pdf (1 page)",
      heading: "Baltic Paper",
      summary: "Baltic Paper · INV-2026-0524 · 2026-11-18 · 3 line items · 812,40 EUR",
      steps: [],
      followUps: EXTRACTION_PROFILE.turns[0].followUps,
    },
    amount: "812,40 EUR",
    share: 0.3,
  },
  {
    turn: {
      question: "invoice-2026-0527.pdf (3 pages)",
      heading: "Rhine Tools",
      summary: "Rhine Tools · INV-2026-0527 · 2026-11-18 · 9 line items · 2.976,10 EUR",
      steps: [],
      followUps: EXTRACTION_PROFILE.turns[0].followUps,
    },
    amount: "2.976,10 EUR",
    share: 0.3,
  },
  {
    turn: {
      question: "invoice-2026-0530.pdf (1 page)",
      heading: "Lyon Bureau",
      summary: "Lyon Bureau · INV-2026-0530 · 2026-11-18 · 2 line items · 540,00 EUR",
      steps: [],
      followUps: EXTRACTION_PROFILE.turns[0].followUps,
    },
    amount: "540,00 EUR",
    share: 0.3,
  },
];

/** The share of the other (M16) vendors' eligible traces that are re-extracted. */
const OTHER_VENDOR_SHARE = 0.05;

/** The vendor of each invoice file name, for the facts (both the M16 and the comma vendors). */
const VENDOR_BY_FILE: ReadonlyMap<string, string> = new Map(
  [...EXTRACTION_PROFILE.turns, ...COMMA_VENDORS.map((vendor) => vendor.turn)].map((turn) => [
    turn.question,
    turn.heading,
  ]),
);

/** The comma vendor of an eligible trace with `index % 20 < 4`; the M16 vendors take the other traces. */
function commaVendorAt(index: number): (typeof COMMA_VENDORS)[number] | undefined {
  return index % 20 < 4 ? COMMA_VENDORS[index % 20] : undefined;
}

/**
 * The vendor turn an eligible trace gets. The comma vendors are set by the marks; the others are what
 * `turnAt` draws without a mark: `pick(profile.turns, index)`, which is `turns[index % turns.length]`.
 */
function vendorTurnAt(index: number): Turn {
  const comma = commaVendorAt(index);
  if (comma) {
    return comma.turn;
  }
  const turns = EXTRACTION_PROFILE.turns;
  return turns[index % turns.length] ?? turns[0];
}

function reExtractionSpans(amount: string | undefined): MarkedSpan[] {
  const reason =
    amount === undefined
      ? "total does not match the line items"
      : `expected a number, got "${amount}"`;
  return [
    {
      name: "validate schema",
      kind: "evaluator",
      fromMs: 3_100,
      durationMs: 40,
      output: { format: "json", value: { valid: false, field: "total", reason } },
    },
    { name: "re-extract fields", kind: "llm", fromMs: 3_200, durationMs: 2_400 },
  ];
}

const SCANNED_ERROR: NonNullable<Mark["error"]> = {
  count: 1,
  spanName: "ocr",
  message: "OCR returned no text",
  stacktrace: "OcrError: no text on any page (scanned image, 0 characters)",
};

const BATCH_ERROR: NonNullable<Mark["error"]> = {
  count: 2,
  spanName: "validate schema",
  message: "schema check failed: total",
  stacktrace: "SchemaError: total does not match the line items",
};

/** The extraction project's marks (M17). Eligible traces only; the same input gives the same marks. */
export function extractionMarks(
  traces: readonly Schema<"TraceSummary">[],
  context: MarksContext,
): ReadonlyMap<number, Mark> {
  const { now, eligible } = context;
  const marks = new Map<number, Mark>();
  const add = (index: number, change: Mark): void => {
    const previous = marks.get(index);
    marks.set(index, previous ? mergeMarks(previous, change) : change);
  };

  const eligibleIndices: number[] = [];
  for (let index = 0; index < traces.length; index += 1) {
    if (eligible(index)) {
      eligibleIndices.push(index);
    }
  }

  // 1. Re-extractions: an even spread per vendor, over its eligible traces in index order.
  const byVendor = new Map<string, number[]>();
  for (const index of eligibleIndices) {
    const comma = commaVendorAt(index);
    if (comma) {
      add(index, { turn: comma.turn });
    }
    const question = vendorTurnAt(index).question;
    byVendor.set(question, [...(byVendor.get(question) ?? []), index]);
  }
  const reExtracted = new Set<number>();
  for (const [question, indices] of byVendor) {
    const comma = COMMA_VENDORS.find((vendor) => vendor.turn.question === question);
    for (const index of evenSpread(indices, comma?.share ?? OTHER_VENDOR_SHARE)) {
      reExtracted.add(index);
      add(index, { tags: ["re-extracted"], spans: reExtractionSpans(comma?.amount) });
    }
  }

  // 2. Scanned PDFs: an even spread over the eligible traces not re-extracted, and a burst in the last hour.
  //    The burst takes the first 12 last-hour traces the spread did not pick.
  const scanCandidates = eligibleIndices.filter((index) => !reExtracted.has(index));
  const scanned = new Set<number>(evenSpread(scanCandidates, 0.03));
  let burst = 0;
  for (const index of scanCandidates) {
    if (burst >= BURST_SIZE) {
      break;
    }
    if (scanned.has(index)) {
      continue;
    }
    const start = Date.parse(traces[index]?.start_time ?? "");
    if (start >= now - HOUR_MS && start < now) {
      scanned.add(index);
      burst += 1;
    }
  }
  for (const index of [...scanned].sort((left, right) => left - right)) {
    add(index, { tags: ["scanned"], error: SCANNED_ERROR });
  }

  // 3. The batch: the first 200 eligible error-free traces that no earlier mark chose and that started before
  // a day ago, one every 18 s over an hour from three hours ago; ranks 197, 198 and 199 fail twice.
  const batch: number[] = [];
  for (const index of eligibleIndices) {
    if (batch.length >= BATCH_SIZE) {
      break;
    }
    const trace = traces[index];
    if (
      !trace ||
      reExtracted.has(index) ||
      scanned.has(index) ||
      trace.error_count !== 0 ||
      Date.parse(trace.start_time) >= now - DAY_MS
    ) {
      continue;
    }
    batch.push(index);
  }
  const batchStart = now - 3 * HOUR_MS;
  batch.forEach((index, rank) => {
    if (BATCH_FAILURE_RANKS.has(rank)) {
      add(index, {
        startMs: batchStart + rank * BATCH_STEP_MS,
        tags: [BATCH_TAG, "re-extracted"],
        error: BATCH_ERROR,
      });
    } else {
      add(index, { startMs: batchStart + rank * BATCH_STEP_MS, tags: [BATCH_TAG] });
    }
  });

  return marks;
}

/** The extraction story profile: the M16 extraction profile with its marks (M17). */
export const EXTRACTION_STORY_PROFILE: Profile = {
  ...EXTRACTION_PROFILE,
  marks: extractionMarks,
};

/** The numbers the extraction pictures quote, from the dataset (M17). */
export type ExtractionFacts = {
  nordicFreightRetryShare: number;
  commaVendors: { "Baltic Paper": number; "Rhine Tools": number; "Lyon Bureau": number };
  otherVendorsRetryShare: number;
  scannedShare: number;
  scannedLastHour: number;
  scannedHourlyBefore: number;
  batchSize: number;
  batchFailedTwice: number;
  batchCost: number;
};

function retryShare(rows: readonly Schema<"TraceSummary">[]): number {
  if (rows.length === 0) {
    return 0;
  }
  return rows.filter((trace) => trace.tags.includes("re-extracted")).length / rows.length;
}

/** The extraction facts of a dataset: eligible traces only (not the showcase, visible by default). */
export function extractionFacts(dataset: Dataset): ExtractionFacts {
  const now = dataset.now;
  // The index is the dataset's order, so the facts never read it from a trace id.
  const traces = dataset.traces.filter((trace, index) =>
    isEligibleTrace(EXTRACTION_STORY_PROFILE, trace, index),
  );
  const vendorOf = (trace: Schema<"TraceSummary">): string | undefined =>
    VENDOR_BY_FILE.get(trace.input_preview ?? "");
  const rowsOf = (vendor: string): Schema<"TraceSummary">[] =>
    traces.filter((trace) => vendorOf(trace) === vendor);
  const commaNames = new Set(COMMA_VENDORS.map((vendor) => vendor.turn.heading));

  const scanned = traces.filter((trace) => trace.tags.includes("scanned"));
  const inLastThirtyDays = traces.filter(
    (trace) => Date.parse(trace.start_time) >= now - 30 * DAY_MS,
  );
  const scannedLastThirty = scanned.filter(
    (trace) => Date.parse(trace.start_time) >= now - 30 * DAY_MS,
  );
  const hourly = new Map<number, number>();
  for (const trace of scanned) {
    const age = now - Date.parse(trace.start_time);
    if (age >= HOUR_MS && age < DAY_MS) {
      const hour = Math.floor(age / HOUR_MS);
      hourly.set(hour, (hourly.get(hour) ?? 0) + 1);
    }
  }

  const batch = traces.filter((trace) => trace.tags.includes(BATCH_TAG));
  return {
    nordicFreightRetryShare: retryShare(rowsOf("Nordic Freight")),
    commaVendors: {
      "Baltic Paper": retryShare(rowsOf("Baltic Paper")),
      "Rhine Tools": retryShare(rowsOf("Rhine Tools")),
      "Lyon Bureau": retryShare(rowsOf("Lyon Bureau")),
    },
    otherVendorsRetryShare: retryShare(
      traces.filter((trace) => {
        const vendor = vendorOf(trace);
        return vendor !== undefined && !commaNames.has(vendor);
      }),
    ),
    scannedShare:
      inLastThirtyDays.length === 0 ? 0 : scannedLastThirty.length / inLastThirtyDays.length,
    scannedLastHour: scanned.filter((trace) => {
      const start = Date.parse(trace.start_time);
      return start >= now - HOUR_MS && start < now;
    }).length,
    // NaN when no hour before the last has a scanned PDF, so a test on the count cannot pass vacuously.
    scannedHourlyBefore: hourly.size > 0 ? Math.max(...hourly.values()) : Number.NaN,
    batchSize: batch.length,
    batchFailedTwice: batch.filter((trace) => trace.error_count >= 2).length,
    batchCost: batch.reduce((sum, trace) => sum + (trace.cost_total ?? 0), 0),
  };
}
