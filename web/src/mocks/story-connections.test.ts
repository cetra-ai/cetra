import { beforeEach, expect, it } from "vitest";
import { server } from "@/mocks/node";
import {
  connectionIdOf,
  STORY_CONNECTION_IDS,
  storyConnectionHandlers,
} from "@/mocks/story-connections";
import connectionsFixture from "../../../fixtures/api/static/llm-connections.json";

beforeEach(() => {
  server.use(...storyConnectionHandlers);
});

type Connection = {
  id: string;
  name: string;
  adapter: string;
  default_model?: string;
  [key: string]: unknown;
};

async function listIn(org: string) {
  const response = await fetch(
    new URL(`/api/v1/orgs/${encodeURIComponent(org)}/llm-connections`, window.location.origin),
  );
  return {
    status: response.status,
    body: (await response.json()) as { data: Connection[]; next_cursor: unknown },
  };
}

it("lists the fixture's OpenAI connection, then an Anthropic and a Google one", async () => {
  const { status, body } = await listIn("acme");
  expect(status).toBe(200);
  expect(body.next_cursor).toBeNull();
  expect(body.data.map((item) => [item.name, item.adapter, item.id])).toEqual([
    ["OpenAI", "openai", "01900000-0000-7000-8000-000000000400"],
    ["Anthropic", "anthropic", "01900000-0000-7000-8000-0000005709a1"],
    ["Google", "gemini", "01900000-0000-7000-8000-0000005709a2"],
  ]);
});

it("gives the new connections the same fields as the fixture's connection", async () => {
  const fixtureKeys = Object.keys(connectionsFixture.data[0] ?? {}).sort();
  const { body } = await listIn("acme");
  expect(body.data.map((item) => Object.keys(item).sort())).toEqual([
    fixtureKeys,
    fixtureKeys,
    fixtureKeys,
  ]);
  expect(body.data.map((item) => item.id)).toEqual([...new Set(body.data.map((item) => item.id))]);
});

it("gives each new connection a default model of its own provider", async () => {
  const { body } = await listIn("acme");
  const [, anthropic, google] = body.data;
  expect(anthropic?.default_model?.startsWith("claude")).toBe(true);
  expect(google?.default_model?.startsWith("gemini")).toBe(true);
});

it("serves the same list in every organization", async () => {
  const first = await listIn("acme");
  const other = await listIn("other-org");
  expect(other.body).toEqual(first.body);
});

it("maps a claude model to Anthropic, a gemini model to Google and a gpt model to OpenAI", () => {
  expect(connectionIdOf("claude-sonnet-4-5")).toBe(STORY_CONNECTION_IDS.anthropic);
  expect(connectionIdOf("claude-haiku-4-5")).toBe(STORY_CONNECTION_IDS.anthropic);
  expect(connectionIdOf("gemini-2.5-flash")).toBe(STORY_CONNECTION_IDS.google);
  expect(connectionIdOf("gpt-4.1-mini")).toBe(STORY_CONNECTION_IDS.openai);
  expect(connectionIdOf("gpt-4o")).toBe(STORY_CONNECTION_IDS.openai);
});

it("names no connection for a model it does not know", () => {
  expect(connectionIdOf("mistral-large")).toBeUndefined();
  expect(connectionIdOf("")).toBeUndefined();
});
