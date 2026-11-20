import { type HttpHandler, HttpResponse, http } from "msw";
import promptsFixture from "../../../fixtures/api/responses/prompts.json";
import issueSourcesFixture from "../../../fixtures/api/static/issue-sources.json";

/**
 * Browser-only answers for the detail pages the contract fixtures do not cover: the fixtures hold
 * one example per route, but the lists link to every row. They answer only what the fixtures miss
 * (a handler that returns nothing falls through to the fixtures), and never in tests.
 */

const PROJECT = "/api/v1/projects/:project";

/** The issue source with a detail fixture of its own. */
const FIXTURE_ISSUE_SOURCE = "01900000-0000-7000-8000-000000001c00";

/** The prompts with fixtures of their own; the others are answered here. */
const FIXTURE_PROMPTS = new Set(["support-reply"]);

type Prompt = (typeof promptsFixture.data)[number];

/** The prompts of the list fixture that have no detail fixtures. */
function promptOf(name: unknown): Prompt | undefined {
  if (typeof name !== "string" || FIXTURE_PROMPTS.has(name)) {
    return undefined;
  }
  return promptsFixture.data.find((prompt) => prompt.name === name);
}

/** The labels that point at `version`, with `latest` first when it is the latest. */
function labelsOf(prompt: Prompt, version: number): string[] {
  return [
    ...(version === prompt.latest_version ? ["latest"] : []),
    ...prompt.labels.filter((label) => label.version === version).map((label) => label.label),
  ];
}

/** Version `version` of a prompt without fixtures: a short template of its type. */
function versionOf(prompt: Prompt, version: number) {
  const chat = prompt.type === "chat";
  return {
    name: prompt.name,
    type: prompt.type,
    version,
    template: chat
      ? [
          { role: "system", content: "You are a helpful assistant." },
          { role: "user", content: "{{ input }}" },
        ]
      : `${prompt.description ?? "Answer"} in a few sentences.\n\n{{ input }}`,
    config: { model: "gpt-5-mini", params: { temperature: 0.2 } },
    labels: labelsOf(prompt, version),
    variables: ["input"],
    placeholders: [],
    commit_message: version === 1 ? "First version" : `Version ${version}`,
    created_at: prompt.created_at,
  };
}

/** The versions of a prompt without fixtures, newest first. */
function versionsOf(prompt: Prompt) {
  return Array.from({ length: prompt.latest_version }, (_, index) =>
    versionOf(prompt, prompt.latest_version - index),
  );
}

/** Plausible per-version numbers for a window, newest version first (spec 09, D-V-17). */
function metricsOf(versions: number[], from: string, to: string) {
  return {
    from,
    to,
    versions: versions.map((version, index) => {
      const spans = 1200 - index * 380;
      return {
        version,
        span_count: spans,
        trace_count: Math.round(spans * 0.8),
        error_count: Math.round(spans * (0.012 + index * 0.01)),
        cost_total: spans * 0.0021,
        cost_avg: 0.0021,
        input_tokens_avg: 640 - index * 40,
        output_tokens_avg: 180 + index * 25,
        latency_p50_ms: 820 + index * 140,
        latency_p95_ms: 2100 + index * 420,
        scores: [{ name: "helpfulness", mean: 0.82 - index * 0.06, count: Math.round(spans / 10) }],
      };
    }),
  };
}

/** The versions a prompt has: the fixture prompt's 2 and 1, or a generated prompt's. */
function versionNumbers(name: unknown): number[] | undefined {
  if (name === "support-reply") {
    return [2, 1];
  }
  const prompt = promptOf(name);
  return prompt ? versionsOf(prompt).map((version) => version.version) : undefined;
}

export const fallbackHandlers: HttpHandler[] = [
  // The metrics fixture answers one exact window; every window gets numbers here.
  http.get(`${PROJECT}/prompts/:prompt/metrics`, ({ request, params }) => {
    const versions = versionNumbers(params.prompt);
    if (!versions) {
      return undefined;
    }
    const url = new URL(request.url);
    const to = url.searchParams.get("to") ?? new Date().toISOString();
    const from = url.searchParams.get("from") ?? to;
    return HttpResponse.json(metricsOf(versions, from, to));
  }),
  http.get(`${PROJECT}/prompts/:prompt/versions/:version`, ({ params }) => {
    const prompt = promptOf(params.prompt);
    const version = Number(params.version);
    if (!prompt || !Number.isInteger(version) || version < 1 || version > prompt.latest_version) {
      return undefined;
    }
    return HttpResponse.json(versionOf(prompt, version));
  }),
  http.get(`${PROJECT}/prompts/:prompt/versions`, ({ params }) => {
    const prompt = promptOf(params.prompt);
    return prompt ? HttpResponse.json({ data: versionsOf(prompt), next_cursor: null }) : undefined;
  }),
  http.get(`${PROJECT}/prompts/:prompt/label-events`, ({ params }) => {
    const prompt = promptOf(params.prompt);
    if (!prompt) {
      return undefined;
    }
    const data = prompt.labels.map((label) => ({
      label: label.label,
      to_version: label.version,
      at: label.updated_at,
    }));
    return HttpResponse.json({ data, next_cursor: null });
  }),
  http.get(`${PROJECT}/prompts/:prompt/resolve`, ({ request, params }) => {
    const prompt = promptOf(params.prompt);
    if (!prompt) {
      return undefined;
    }
    const wanted = new URL(request.url).searchParams.get("label") ?? "production";
    const label = prompt.labels.find((item) => item.label === wanted);
    return HttpResponse.json(versionOf(prompt, label?.version ?? prompt.latest_version));
  }),
  http.get(`${PROJECT}/prompts/:prompt`, ({ params }) => {
    const prompt = promptOf(params.prompt);
    return prompt ? HttpResponse.json(prompt) : undefined;
  }),
  // Every issue source of the list opens; the fixture has the first one's detail only.
  http.get(`${PROJECT}/issue-sources/:id`, ({ params }) => {
    if (params.id === FIXTURE_ISSUE_SOURCE) {
      return undefined;
    }
    const source = issueSourcesFixture.data.find((item) => item.id === params.id);
    return source ? HttpResponse.json(source) : undefined;
  }),
];
