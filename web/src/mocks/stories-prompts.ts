import { type HttpHandler, HttpResponse, http } from "msw";
import type { Schema } from "@/api/types";
import promptsFixture from "../../../fixtures/api/responses/prompts.json";
import { metricsOf } from "./fallbacks";
import { projectOfRef, type StoryId } from "./projects";

/**
 * The prompts of the homepage stories (design §12, M14), answered in the browser only. The fixture
 * prompts keep their own answers: these handlers return nothing for any other name, so the fallbacks
 * and the contract fixtures answer them. Never installed in tests; a test calls `server.use`.
 */

const PROJECT = "/api/v1/projects/:project";

/** A path parameter as text; msw types it as possibly repeated or missing. */
function textParam(value: string | readonly string[] | undefined): string {
  return typeof value === "string" ? value : "";
}

type Template = { role: "system" | "user"; content: string }[];

/** One version of a story prompt, oldest first in `STORY_PROMPTS`. */
type StoryVersion = {
  template: Template;
  variables: string[];
  model: string;
  temperature: number;
  maxTokens: number;
  commit: string;
  at: string;
};

type StoryPrompt = {
  id: string;
  /** The story whose project serves the prompt. */
  story: StoryId;
  name: string;
  description: string;
  tags: string[];
  versions: StoryVersion[];
};

export const STORY_PROMPTS: StoryPrompt[] = [
  {
    id: "01900000-0000-7000-8000-000000570702",
    story: "coding",
    name: "write-patch",
    description: "Turn an issue and a file into a unified diff",
    tags: ["coding"],
    versions: [
      {
        template: [
          { role: "system", content: "You are a coding agent. Write a unified diff." },
          { role: "user", content: "Issue: {{ issue }}\n\n{{ code }}" },
        ],
        variables: ["issue", "code"],
        model: "claude-haiku-4-5",
        temperature: 0.2,
        maxTokens: 1024,
        commit: "First version",
        at: "2026-10-12T10:15:00.000000000Z",
      },
      {
        template: [
          {
            role: "system",
            content: "You are a coding agent. Write a unified diff that fixes the issue.",
          },
          { role: "user", content: "Issue: {{ issue }}\n\n{{ code }}" },
        ],
        variables: ["issue", "code"],
        model: "claude-sonnet-4-5",
        temperature: 0.2,
        maxTokens: 1024,
        commit: "Move to Sonnet and ask for a diff that fixes the issue",
        at: "2026-10-19T14:40:00.000000000Z",
      },
      {
        template: [
          {
            role: "system",
            content:
              "You are a coding agent. Write a unified diff that fixes the issue, and nothing else.",
          },
          { role: "user", content: "Issue: {{ issue }}\n\nFile {{ path }}:\n{{ code }}" },
        ],
        variables: ["issue", "path", "code"],
        model: "claude-sonnet-4-5",
        temperature: 0,
        maxTokens: 1024,
        commit: "Name the file and ask for the diff only",
        at: "2026-10-29T09:05:00.000000000Z",
      },
    ],
  },
  {
    id: "01900000-0000-7000-8000-000000570703",
    story: "voice",
    name: "voice-reply",
    description: "What the receptionist says back on the phone",
    tags: ["voice"],
    versions: [
      {
        template: [
          { role: "system", content: "You are a receptionist. Answer the caller." },
          { role: "user", content: "{{ caller }}" },
        ],
        variables: ["caller"],
        model: "gpt-4.1-mini",
        temperature: 0.5,
        maxTokens: 256,
        commit: "First version",
        at: "2026-10-10T16:20:00.000000000Z",
      },
      {
        template: [
          {
            role: "system",
            content: "You are a receptionist on the phone. Answer in two sentences.",
          },
          { role: "user", content: "{{ caller }}" },
        ],
        variables: ["caller"],
        model: "gpt-4.1-mini",
        temperature: 0.4,
        maxTokens: 256,
        commit: "Keep replies short for speech",
        at: "2026-10-16T11:00:00.000000000Z",
      },
      {
        template: [
          {
            role: "system",
            content: "You are a receptionist on the phone. Answer in two sentences.",
          },
          { role: "user", content: "{{ caller }}\n\nFree slots: {{ slots }}" },
        ],
        variables: ["caller", "slots"],
        model: "gpt-4.1-mini",
        temperature: 0.4,
        maxTokens: 256,
        commit: "Offer the free slots",
        at: "2026-10-22T15:30:00.000000000Z",
      },
      {
        template: [
          {
            role: "system",
            content: "You are a receptionist on the phone. Answer in one or two short sentences.",
          },
          { role: "user", content: "{{ caller }}\n\nFree slots: {{ slots }}" },
        ],
        variables: ["caller", "slots"],
        model: "gpt-4.1-mini",
        temperature: 0.3,
        maxTokens: 120,
        commit: "One or two sentences, so the reply starts sooner",
        at: "2026-10-30T10:10:00.000000000Z",
      },
    ],
  },
  {
    id: "01900000-0000-7000-8000-000000570704",
    story: "extraction",
    name: "extract-invoice",
    description: "Read an invoice's fields from its text",
    tags: ["billing"],
    versions: [
      {
        template: [
          { role: "system", content: "Extract the invoice as JSON." },
          { role: "user", content: "{{ ocr_text }}" },
        ],
        variables: ["ocr_text"],
        model: "gpt-4o-mini",
        temperature: 0,
        maxTokens: 512,
        commit: "First version",
        at: "2026-09-20T09:00:00.000000000Z",
      },
      {
        template: [
          {
            role: "system",
            content: "Extract the invoice as JSON: vendor, invoice_number, date, total.",
          },
          { role: "user", content: "{{ ocr_text }}" },
        ],
        variables: ["ocr_text"],
        model: "gpt-4o-mini",
        temperature: 0,
        maxTokens: 512,
        commit: "Name the fields",
        at: "2026-09-27T13:20:00.000000000Z",
      },
      {
        template: [
          {
            role: "system",
            content:
              "Extract the invoice as JSON: vendor, invoice_number, date, currency, line_items, total.",
          },
          { role: "user", content: "{{ ocr_text }}" },
        ],
        variables: ["ocr_text"],
        model: "gemini-2.5-flash",
        temperature: 0.2,
        maxTokens: 512,
        commit: "Add the currency and the line items",
        at: "2026-10-04T10:45:00.000000000Z",
      },
      {
        template: [
          {
            role: "system",
            content:
              "Extract the invoice as JSON: vendor, invoice_number, date, currency, line_items, total.",
          },
          { role: "user", content: "{{ ocr_text }}" },
        ],
        variables: ["ocr_text"],
        model: "gemini-2.5-flash",
        temperature: 0,
        maxTokens: 512,
        commit: "Lower the temperature",
        at: "2026-10-15T08:30:00.000000000Z",
      },
      {
        template: [
          {
            role: "system",
            content:
              "Extract the invoice as JSON: vendor, invoice_number, date, currency, line_items, total.",
          },
          { role: "user", content: "{{ ocr_text }}" },
        ],
        variables: ["ocr_text"],
        model: "gemini-2.5-flash",
        temperature: 0,
        maxTokens: 600,
        commit: "Cap the output at 600 tokens",
        at: "2026-10-29T15:50:00.000000000Z",
      },
    ],
  },
];

/** The story prompt called `name` in the project `project` (an API ref), if any: a story prompt is served only in its story's project. */
function storyPrompt(name: unknown, project: string): StoryPrompt | undefined {
  if (typeof name !== "string") {
    return undefined;
  }
  const prompt = STORY_PROMPTS.find((candidate) => candidate.name === name);
  return prompt !== undefined && projectOfRef(project)?.story === prompt.story ? prompt : undefined;
}

/** The prompt's detail: its latest version is the last one, labelled `production`. */
function promptOf(story: StoryPrompt): Schema<"Prompt"> {
  const last = story.versions[story.versions.length - 1];
  const first = story.versions[0];
  return {
    id: story.id,
    name: story.name,
    type: "chat",
    description: story.description,
    tags: story.tags,
    latest_version: story.versions.length,
    labels: [
      {
        label: "production",
        version: story.versions.length,
        updated_at: last?.at ?? "",
      },
    ],
    created_at: first?.at ?? "",
    updated_at: last?.at ?? "",
  };
}

/** Version `version` (1-based) of a story prompt, or undefined when there is none. */
function versionOf(story: StoryPrompt, version: number): Schema<"PromptVersion"> | undefined {
  const latest = story.versions.length;
  const v = story.versions[version - 1];
  if (v === undefined) {
    return undefined;
  }
  return {
    name: story.name,
    type: "chat",
    version,
    template: v.template,
    config: { model: v.model, params: { temperature: v.temperature, max_tokens: v.maxTokens } },
    labels: version === latest ? ["latest", "production"] : [],
    variables: v.variables,
    placeholders: [],
    commit_message: v.commit,
    created_at: v.at,
  };
}

/** Every version of a story prompt, newest first. */
function versionsOf(story: StoryPrompt): Schema<"PromptVersion">[] {
  return story.versions
    .map((_, index) => versionOf(story, story.versions.length - index))
    .filter((version): version is Schema<"PromptVersion"> => version !== undefined);
}

/** One event per version, newest first; a version after the first records the one it moved from. */
function labelEventsOf(story: StoryPrompt) {
  return story.versions
    .map((v, index) => {
      const to = index + 1;
      return to > 1
        ? { label: "production", to_version: to, from_version: to - 1, at: v.at }
        : { label: "production", to_version: to, at: v.at };
    })
    .reverse();
}

export const storyPromptHandlers: HttpHandler[] = [
  // Each story route below returns nothing for a prompt that is not a story, so the fixtures answer it.
  http.get(`${PROJECT}/prompts/:prompt/metrics`, ({ request, params }) => {
    const story = storyPrompt(params.prompt, textParam(params.project));
    if (story === undefined) {
      return undefined;
    }
    const url = new URL(request.url);
    const to = url.searchParams.get("to") ?? new Date().toISOString();
    const from = url.searchParams.get("from") ?? to;
    const versions = story.versions.map((_, index) => story.versions.length - index);
    return HttpResponse.json(metricsOf(versions, from, to));
  }),
  http.get(`${PROJECT}/prompts/:prompt/versions/:version`, ({ params }) => {
    const story = storyPrompt(params.prompt, textParam(params.project));
    if (story === undefined) {
      return undefined;
    }
    const version = Number(params.version);
    if (!Number.isInteger(version) || version < 1 || version > story.versions.length) {
      return undefined;
    }
    const detail = versionOf(story, version);
    return detail ? HttpResponse.json(detail) : undefined;
  }),
  http.get(`${PROJECT}/prompts/:prompt/versions`, ({ params }) => {
    const story = storyPrompt(params.prompt, textParam(params.project));
    if (story === undefined) {
      return undefined;
    }
    return HttpResponse.json({ data: versionsOf(story), next_cursor: null });
  }),
  http.get(`${PROJECT}/prompts/:prompt/label-events`, ({ params }) => {
    const story = storyPrompt(params.prompt, textParam(params.project));
    if (story === undefined) {
      return undefined;
    }
    return HttpResponse.json({ data: labelEventsOf(story), next_cursor: null });
  }),
  http.get(`${PROJECT}/prompts/:prompt/resolve`, ({ request, params }) => {
    const story = storyPrompt(params.prompt, textParam(params.project));
    if (story === undefined) {
      return undefined;
    }
    const wanted = new URL(request.url).searchParams.get("label") ?? "production";
    if (wanted !== "production" && wanted !== "latest") {
      return undefined;
    }
    const detail = versionOf(story, story.versions.length);
    return detail ? HttpResponse.json(detail) : undefined;
  }),
  http.get(`${PROJECT}/prompts/:prompt`, ({ params }) => {
    const story = storyPrompt(params.prompt, textParam(params.project));
    return story === undefined ? undefined : HttpResponse.json(promptOf(story));
  }),
  // The fixture prompts first, then the story prompts of this project; the filters apply to both.
  http.get(`${PROJECT}/prompts`, ({ request, params }) => {
    // The fixture is checked against the Prompt schema by the contract tests.
    const fixtures = promptsFixture.data as Schema<"Prompt">[];
    const url = new URL(request.url);
    const q = url.searchParams.get("q")?.toLowerCase();
    const label = url.searchParams.get("label");
    const tag = url.searchParams.get("tag");
    const project = textParam(params.project);
    const stories = STORY_PROMPTS.filter((prompt) => projectOfRef(project)?.story === prompt.story);
    const data = [...fixtures, ...stories.map(promptOf)].filter(
      (prompt) =>
        (!q || prompt.name.toLowerCase().includes(q)) &&
        (!label || prompt.labels.some((item) => item.label === label)) &&
        (!tag || prompt.tags.includes(tag)),
    );
    return HttpResponse.json({ data, next_cursor: null });
  }),
];
