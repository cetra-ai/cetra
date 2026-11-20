import { beforeEach, expect, it } from "vitest";
import {
  ADA_ID,
  BOB_ID,
  FIRING_RULE_ID,
  NEW_USER_ID,
  SLACK_CHANNEL_ID,
  TOKEN_ID,
} from "@/test/fixtures";
import openapiDocument from "../../../crates/server/openapi.json";
import adminUserCreatedDocument from "../../../fixtures/api/static/admin-user-created.json";
import inviteCreatedDocument from "../../../fixtures/api/static/invite-created.json";
import { server } from "./node";
import { WRITE_OPERATIONS, writeHandlers } from "./writes";

const PROJECT = "/api/v1/projects/acme%2Fsupport-bot";
const BASELINE_ID = "01900000-0000-7000-8000-000000000e01";
const CONNECTION_ID = "01900000-0000-7000-8000-000000000400";
const ITEM_ID = "01900000-0000-7000-8000-000000000d01";

/** Writes that later sub-phases add under the covered prefixes. */
const LATER_TRACKS: string[] = [];

beforeEach(() => {
  server.use(...writeHandlers);
});

function part(json: Record<string, unknown>, key: string): Record<string, unknown> {
  const value = json[key];
  if (typeof value !== "object" || value === null || Array.isArray(value)) {
    throw new Error(`${key} is not an object`);
  }
  return value as Record<string, unknown>;
}

async function send(
  method: string,
  path: string,
  requestBody?: string,
): Promise<{ status: number; json: Record<string, unknown> | undefined }> {
  const response = await fetch(new URL(path, window.location.origin), {
    method,
    ...(requestBody !== undefined
      ? { body: requestBody, headers: { "Content-Type": "application/json" } }
      : {}),
  });
  const text = await response.text();
  return {
    status: response.status,
    json: text ? (JSON.parse(text) as Record<string, unknown>) : undefined,
  };
}

it("lists every account and admin write of the contract", () => {
  const paths = openapiDocument.paths as unknown as Record<
    string,
    Record<string, { operationId?: string }>
  >;
  const expected: { operationId: string; method: string; path: string }[] = [];
  for (const [path, operations] of Object.entries(paths)) {
    for (const method of ["post", "patch", "put", "delete"] as const) {
      const operationId = operations[method]?.operationId;
      if (!operationId) {
        continue;
      }
      const covered =
        path.startsWith("/api/auth/") ||
        path.startsWith("/api/v1/me") ||
        path.startsWith("/api/v1/admin/") ||
        path.startsWith("/api/v1/orgs") ||
        path === "/api/v1/projects/{project}" ||
        path.startsWith("/api/v1/projects/{project}/api-keys") ||
        path.startsWith("/api/v1/projects/{project}/deletions") ||
        path.startsWith("/api/v1/projects/{project}/members") ||
        path.startsWith("/api/v1/projects/{project}/evaluators") ||
        path.startsWith("/api/v1/projects/{project}/score-definitions") ||
        path.startsWith("/api/v1/projects/{project}/models") ||
        path.startsWith("/api/v1/projects/{project}/channels") ||
        path.startsWith("/api/v1/projects/{project}/alert-rules") ||
        path.startsWith("/api/v1/projects/{project}/online-rules") ||
        path.startsWith("/api/v1/projects/{project}/datasets") ||
        path.startsWith("/api/v1/projects/{project}/scores") ||
        path.startsWith("/api/v1/projects/{project}/experiments") ||
        path.startsWith("/api/v1/projects/{project}/prompts") ||
        path.startsWith("/api/v1/projects/{project}/prompt-settings") ||
        path.startsWith("/api/v1/projects/{project}/annotation-queues");
      if (covered && !LATER_TRACKS.includes(operationId)) {
        expected.push({ operationId, method, path });
      }
    }
  }
  expect(expected.map((operation) => operation.operationId).sort()).toEqual(
    WRITE_OPERATIONS.map((operation) => operation.operationId).sort(),
  );
  const listed = new Map(WRITE_OPERATIONS.map((operation) => [operation.operationId, operation]));
  for (const operation of expected) {
    const match = listed.get(operation.operationId);
    expect(match, operation.operationId).toBeDefined();
    expect(match?.method, operation.operationId).toBe(operation.method);
    expect(match?.path, operation.operationId).toBe(operation.path);
  }
});

it("has one handler per operation", () => {
  expect(writeHandlers).toHaveLength(WRITE_OPERATIONS.length);
});

it("signs in as the fixture user", async () => {
  const response = await send(
    "POST",
    "/api/auth/login",
    JSON.stringify({ email: "ada@example.com", password: "x" }),
  );
  expect(response.status).toBe(200);
  expect(part(response.json ?? {}, "user").email).toBe("ada@example.com");
});

it("refuses to change the signed-in admin", async () => {
  const response = await send(
    "PATCH",
    `/api/v1/admin/users/${ADA_ID}`,
    JSON.stringify({ disabled: true }),
  );
  expect(response.status).toBe(400);
  expect(response.json?.detail).toBe("cannot change your own account");
});

it("saves the org assistant setting and stamps it", async () => {
  const response = await send(
    "PUT",
    "/api/v1/orgs/acme/assistant",
    JSON.stringify({ enabled: true, connection_id: CONNECTION_ID, model: "gpt-4o" }),
  );
  expect(response.status).toBe(200);
  expect(response.json).toMatchObject({
    enabled: true,
    connection_id: CONNECTION_ID,
    model: "gpt-4o",
  });
  expect(typeof response.json?.updated_at).toBe("string");
});

it("refuses to turn the org assistant on without a connection", async () => {
  const response = await send(
    "PUT",
    "/api/v1/orgs/acme/assistant",
    JSON.stringify({ enabled: true, model: "gpt-4o" }),
  );
  expect(response.status).toBe(400);
  expect(response.json?.errors).toEqual([
    { field: "connection_id", message: "required when enabled" },
  ]);
});

it("answers the org assistant test with the test result", async () => {
  const response = await send(
    "POST",
    "/api/v1/orgs/acme/assistant/test",
    JSON.stringify({ connection_id: CONNECTION_ID, model: "gpt-5-mini" }),
  );
  expect(response.status).toBe(200);
  expect(response.json?.ok).toBe(true);
});

it("creates a public key", async () => {
  const response = await send(
    "POST",
    `${PROJECT}/api-keys`,
    JSON.stringify({ kind: "public", name: "Web" }),
  );
  expect(response.status).toBe(201);
  const key = part(response.json ?? {}, "key");
  expect(key.kind).toBe("public");
  expect(key.scopes).toEqual(["feedback"]);
  expect(key.name).toBe("Web");
  expect(response.json?.secret).toMatch(/^pk-cetra-/);
});

it("omits the name of an unnamed key", async () => {
  const response = await send("POST", `${PROJECT}/api-keys`, JSON.stringify({ kind: "secret" }));
  expect(response.status).toBe(201);
  expect("name" in part(response.json ?? {}, "key")).toBe(false);
});

it("refuses an override for an owner", async () => {
  const response = await send(
    "PUT",
    `${PROJECT}/members/${ADA_ID}`,
    JSON.stringify({ role: "viewer" }),
  );
  expect(response.status).toBe(400);
});

it("accepts a deletion", async () => {
  const response = await send(
    "POST",
    `${PROJECT}/deletions`,
    JSON.stringify({ kind: "user", user_id: "user-3" }),
  );
  expect(response.status).toBe(202);
  expect(response.json?.status).toBe("resolving");
});

it("accepts a project drop", async () => {
  const response = await send("DELETE", `${PROJECT}?confirm=support-bot`);
  expect(response.status).toBe(202);
  expect(response.json?.kind).toBe("project");
});

it("accepts an org drop", async () => {
  const response = await send("DELETE", "/api/v1/orgs/acme?confirm=acme");
  expect(response.status).toBe(202);
  expect(response.json?.kind).toBe("org");
});

it("answers the channel and alert rule writes", async () => {
  const channel = await send(
    "POST",
    `${PROJECT}/channels`,
    JSON.stringify({ name: "  Pager  ", kind: "webhook" }),
  );
  expect(channel.status).toBe(201);
  expect(channel.json?.name).toBe("Pager");

  const renamedChannel = await send(
    "PATCH",
    `${PROJECT}/channels/${SLACK_CHANNEL_ID}`,
    JSON.stringify({ name: "Renamed" }),
  );
  expect(renamedChannel.status).toBe(200);
  expect(renamedChannel.json?.name).toBe("Renamed");

  const tested = await send("POST", `${PROJECT}/channels/${SLACK_CHANNEL_ID}/test`);
  expect(tested.status).toBe(200);
  expect(tested.json).toEqual({ ok: true });

  const rule = await send(
    "POST",
    `${PROJECT}/alert-rules`,
    JSON.stringify({
      name: "Errors",
      condition: { metric: "error_rate", window: "5m", comparator: "gt", threshold: 0.05 },
      enabled: false,
    }),
  );
  expect(rule.status).toBe(201);
  expect(rule.json?.name).toBe("Errors");
  expect(rule.json?.enabled).toBe(false);
  expect(rule.json?.state).toBe("ok");
  expect("last_value" in (rule.json ?? {})).toBe(false);

  const renamedRule = await send(
    "PATCH",
    `${PROJECT}/alert-rules/${FIRING_RULE_ID}`,
    JSON.stringify({ name: "Errors 2" }),
  );
  expect(renamedRule.status).toBe(200);
  expect(renamedRule.json?.name).toBe("Errors 2");

  expect((await send("DELETE", `${PROJECT}/channels/${SLACK_CHANNEL_ID}`)).status).toBe(204);
  expect((await send("DELETE", `${PROJECT}/alert-rules/${FIRING_RULE_ID}`)).status).toBe(204);
});

it("refuses a direction on a text definition and a numeric one without a range", async () => {
  const text = await send(
    "POST",
    `${PROJECT}/score-definitions`,
    JSON.stringify({ name: "summary", data_type: "text", direction: "higher" }),
  );
  expect(text.status).toBe(400);
  expect(text.json?.errors).toEqual([
    { field: "direction", message: "a text score has no direction" },
  ]);

  const unranged = await send(
    "POST",
    `${PROJECT}/score-definitions`,
    JSON.stringify({ name: "accuracy", data_type: "numeric", direction: "lower" }),
  );
  expect(unranged.status).toBe(400);
  expect(unranged.json?.errors).toEqual([
    { field: "direction", message: "direction needs min_value and max_value" },
  ]);

  const created = await send(
    "POST",
    `${PROJECT}/score-definitions`,
    JSON.stringify({ name: "accuracy", data_type: "numeric", min_value: 0, max_value: 1 }),
  );
  expect(created.status).toBe(201);
  expect(created.json?.direction).toBe("none");
});

it("answers the score writes", async () => {
  const created = await send(
    "POST",
    `${PROJECT}/scores`,
    JSON.stringify({
      scores: [
        {
          target: { type: "trace", trace_id: "a1000000000000000000000000000001" },
          name: "tone",
          value: { data_type: "boolean", value: true },
        },
        {
          target: { type: "session", session_id: "sess-001" },
          name: "note",
          value: { data_type: "text", value: "ok" },
        },
      ],
    }),
  );
  expect(created.status).toBe(200);
  expect(created.json?.score_ids).toHaveLength(2);

  const deleted = await send("DELETE", `${PROJECT}/scores/01900000-0000-7000-8000-000000000310`);
  expect(deleted.status).toBe(204);
});

it("creates a dataset with the given name", async () => {
  const response = await send("POST", `${PROJECT}/datasets`, JSON.stringify({ name: "golden" }));
  expect(response.status).toBe(201);
  expect(response.json?.name).toBe("golden");
});

it("answers the batch edit, batch delete, restore and bulk trace writes", async () => {
  const edited = await send(
    "PATCH",
    `${PROJECT}/datasets/faq/items`,
    JSON.stringify({ items: [{ id: ITEM_ID, metadata: { topic: "exports" } }] }),
  );
  expect(edited.status).toBe(200);
  expect(part(edited.json ?? {}, "version").edited).toBe(1);
  expect(Array.isArray(edited.json?.items)).toBe(true);

  const deleted = await send(
    "POST",
    `${PROJECT}/datasets/faq/items/delete`,
    JSON.stringify({ ids: [ITEM_ID] }),
  );
  expect(deleted.status).toBe(200);
  expect(deleted.json?.deleted).toBe(2);

  const restored = await send(
    "POST",
    `${PROJECT}/datasets/faq/restore`,
    JSON.stringify({ version: 1 }),
  );
  expect(restored.status).toBe(200);
  expect(restored.json?.description).toBe("restore version 1");

  const copied = await send(
    "POST",
    `${PROJECT}/datasets/faq/items/from-traces`,
    JSON.stringify({ items: [{ trace_id: "a1000000000000000000000000000001" }] }),
  );
  expect(copied.status).toBe(201);
  expect(part(copied.json ?? {}, "version").added).toBe(1);
  expect(Array.isArray(copied.json?.skipped)).toBe(true);
});

it("answers the experiment writes", async () => {
  const created = await send(
    "POST",
    `${PROJECT}/experiments`,
    JSON.stringify({ name: "candidate", dataset: "faq" }),
  );
  expect(created.status).toBe(201);
  expect(part(created.json ?? {}, "experiment").name).toBe("candidate");

  const stored = await send(
    "POST",
    `${PROJECT}/experiments/${BASELINE_ID}/items`,
    JSON.stringify({ results: [{ dataset_item_id: ITEM_ID, repetition: 0 }] }),
  );
  expect(stored.status).toBe(200);
  expect(stored.json).toEqual({ stored: 1, skipped: 0 });

  const finished = await send("POST", `${PROJECT}/experiments/${BASELINE_ID}/finish`, "{}");
  expect(finished.status).toBe(200);
  expect(finished.json?.status).toBe("completed");
  expect(finished.json?.id).toBe(BASELINE_ID);

  const deleted = await send("DELETE", `${PROJECT}/experiments/${BASELINE_ID}`);
  expect(deleted.status).toBe(204);
});

it("answers the annotation queue writes", async () => {
  const queue = `${PROJECT}/annotation-queues`;
  const queueId = "01900000-0000-7000-8000-000000000a01";
  const itemId = "01900000-0000-7000-8000-000000000a11";

  const created = await send(
    "POST",
    queue,
    JSON.stringify({ name: "  Review  ", definition_ids: [], target: "trace" }),
  );
  expect(created.status).toBe(201);
  expect(created.json?.name).toBe("Review");

  const updated = await send("PATCH", `${queue}/${queueId}`, JSON.stringify({ version: 2 }));
  expect(updated.status).toBe(200);
  expect(updated.json?.version).toBe(3);

  const added = await send(
    "POST",
    `${queue}/${queueId}/items`,
    JSON.stringify({ items: [{ trace_id: "a1000000000000000000000000000001" }] }),
  );
  expect(added.status).toBe(201);
  expect(added.json?.added).toBe(1);

  const next = await send("POST", `${queue}/${queueId}/next`, "{}");
  expect(next.status).toBe(200);
  expect(next.json?.id).toBe(itemId);

  const completed = await send("POST", `${queue}/${queueId}/items/${itemId}/complete`);
  expect(completed.json?.status).toBe("completed");

  const skipped = await send("POST", `${queue}/${queueId}/items/${itemId}/skip`, "{}");
  expect(skipped.json?.status).toBe("skipped");

  const preview = await send("POST", `${queue}/${queueId}/items/matching/preview`, "{}");
  expect(preview.json).toEqual({ matched: 42, capped: false });

  expect((await send("DELETE", `${queue}/${queueId}/items/${itemId}/lease`)).status).toBe(204);
  expect((await send("DELETE", `${queue}/${queueId}`)).status).toBe(204);
});

it("answers 204 for deletes", async () => {
  expect((await send("DELETE", `/api/v1/me/tokens/${TOKEN_ID}`)).status).toBe(204);
  expect((await send("DELETE", "/api/v1/me/tokens/current")).status).toBe(204);
});

it("keeps retention when the patch leaves it out", async () => {
  const renamed = await send("PATCH", PROJECT, JSON.stringify({ name: "Renamed" }));
  expect(renamed.status).toBe(200);
  expect(renamed.json?.name).toBe("Renamed");
  expect(renamed.json?.retention_days).toBe(30);

  const forever = await send("PATCH", PROJECT, JSON.stringify({ retention_days: null }));
  expect(forever.status).toBe(200);
  expect("retention_days" in (forever.json ?? {})).toBe(false);

  const week = await send("PATCH", PROJECT, JSON.stringify({ retention_days: 7 }));
  expect(week.status).toBe(200);
  expect(week.json?.retention_days).toBe(7);
});

type Check = (json: Record<string, unknown>) => void;

const rows: [string, string, string, string | undefined, number, Check?][] = [
  [
    "patch_me trims the name",
    "PATCH",
    "/api/v1/me",
    JSON.stringify({ name: "  Ada  " }),
    200,
    (json) => expect(json.name).toBe("Ada"),
  ],
  [
    "patch_me clears the name",
    "PATCH",
    "/api/v1/me",
    JSON.stringify({}),
    200,
    (json) => expect("name" in json).toBe(false),
  ],
  [
    "post_personal_token keeps the expiry",
    "POST",
    "/api/v1/me/tokens",
    JSON.stringify({ name: "CI", scopes: ["read"], expires_at: "2027-01-01T00:00:00Z" }),
    201,
    (json) => {
      const token = part(json, "token");
      expect(token.name).toBe("CI");
      expect(token.scopes).toEqual(["read"]);
      expect(token.expires_at).toBe("2027-01-01T00:00:00.000Z");
    },
  ],
  [
    "post_personal_token drops a bad expiry",
    "POST",
    "/api/v1/me/tokens",
    JSON.stringify({ name: "CI", scopes: ["read"], expires_at: "soon" }),
    201,
    (json) => expect("expires_at" in part(json, "token")).toBe(false),
  ],
  [
    "post_org names the org",
    "POST",
    "/api/v1/orgs",
    JSON.stringify({ name: "Beta" }),
    201,
    (json) => {
      expect(json.name).toBe("Beta");
      expect(json.slug).toBe("acme");
    },
  ],
  [
    "patch_org renames",
    "PATCH",
    "/api/v1/orgs/acme",
    JSON.stringify({ name: "Acme 2" }),
    200,
    (json) => expect(json.name).toBe("Acme 2"),
  ],
  [
    "patch_org_member sets a role",
    "PATCH",
    `/api/v1/orgs/acme/members/${BOB_ID}`,
    JSON.stringify({ role: "viewer" }),
    200,
    (json) => expect(json.role).toBe("viewer"),
  ],
  [
    "patch_org_member refuses a bad role",
    "PATCH",
    `/api/v1/orgs/acme/members/${BOB_ID}`,
    JSON.stringify({ role: "boss" }),
    400,
  ],
  [
    "patch_org_member answers 404",
    "PATCH",
    `/api/v1/orgs/acme/members/${NEW_USER_ID}`,
    JSON.stringify({ role: "viewer" }),
    404,
  ],
  [
    "post_org_invite echoes",
    "POST",
    "/api/v1/orgs/acme/invites",
    JSON.stringify({ email: "x@example.com", role: "viewer" }),
    201,
    (json) => {
      const invite = part(json, "invite");
      expect(invite.email).toBe("x@example.com");
      expect(invite.role).toBe("viewer");
      expect(json.accept_url).toBe(inviteCreatedDocument.accept_url);
    },
  ],
  [
    "post_project keeps retention",
    "POST",
    "/api/v1/orgs/acme/projects",
    JSON.stringify({ name: "New", retention_days: 7 }),
    201,
    (json) => {
      expect(json.name).toBe("New");
      expect(json.retention_days).toBe(7);
    },
  ],
  [
    "put_project_member hides with none",
    "PUT",
    `${PROJECT}/members/${BOB_ID}`,
    JSON.stringify({ role: "none" }),
    200,
    (json) => {
      expect(json.override).toBe("none");
      expect("effective_role" in json).toBe(false);
    },
  ],
  [
    "put_project_member answers 404",
    "PUT",
    `${PROJECT}/members/${NEW_USER_ID}`,
    JSON.stringify({ role: "viewer" }),
    404,
  ],
  [
    "post_admin_user names and promotes",
    "POST",
    "/api/v1/admin/users",
    JSON.stringify({ email: "n@example.com", name: " N ", is_instance_admin: true }),
    201,
    (json) => {
      const user = part(json, "user");
      expect(user.email).toBe("n@example.com");
      expect(user.name).toBe("N");
      expect(user.is_instance_admin).toBe(true);
    },
  ],
  [
    "post_admin_user without a name",
    "POST",
    "/api/v1/admin/users",
    JSON.stringify({ email: "n@example.com" }),
    201,
    (json) => {
      const user = part(json, "user");
      expect("name" in user).toBe(false);
      expect(user.is_instance_admin).toBe(false);
    },
  ],
  [
    "patch_admin_user disables Bob",
    "PATCH",
    `/api/v1/admin/users/${BOB_ID}`,
    JSON.stringify({ disabled: true }),
    200,
    (json) => expect(json.disabled).toBe(true),
  ],
  [
    "patch_admin_user answers 404",
    "PATCH",
    `/api/v1/admin/users/${NEW_USER_ID}`,
    JSON.stringify({ disabled: true }),
    404,
  ],
  [
    "reset link for Bob",
    "POST",
    `/api/v1/admin/users/${BOB_ID}/reset-link`,
    undefined,
    200,
    (json) => expect(json.url).toBe(adminUserCreatedDocument.reset_url),
  ],
  [
    "test email without SMTP",
    "POST",
    "/api/v1/admin/settings/test-email",
    undefined,
    400,
    (json) => expect(json.detail).toBe("email is not configured"),
  ],
];

it.each(rows)(
  "%s",
  async (_label: string, method: string, path: string, requestBody:
    | string
    | undefined, status: number, check?: Check) => {
    const response = await send(method, path, requestBody);
    expect(response.status).toBe(status);
    check?.(response.json ?? {});
  },
);

it("answers the project model price writes", async () => {
  const created = await send(
    "POST",
    `${PROJECT}/models`,
    JSON.stringify({ model_name: "  Local  ", price_input: 0, price_output: 0 }),
  );
  expect(created.status).toBe(201);
  expect(created.json?.model_name).toBe("Local");

  const id = "01900000-0000-7000-8000-000000000c01";
  const patched = await send(
    "PATCH",
    `${PROJECT}/models/${id}`,
    JSON.stringify({ model_name: "Local", price_input: 0, price_output: 0 }),
  );
  expect(patched.status).toBe(200);
  expect(patched.json?.id).toBe(id);

  const deleted = await send("DELETE", `${PROJECT}/models/${id}`);
  expect(deleted.status).toBe(204);
});

it("answers the org model price writes", async () => {
  const created = await send(
    "POST",
    "/api/v1/orgs/acme/models",
    JSON.stringify({ model_name: "  Local  ", price_input: 0, price_output: 0 }),
  );
  expect(created.status).toBe(201);
  expect(created.json?.model_name).toBe("Local");

  const id = "01900000-0000-7000-8000-000000000c00";
  const patched = await send(
    "PATCH",
    `/api/v1/orgs/acme/models/${id}`,
    JSON.stringify({ model_name: "Local", price_input: 0, price_output: 0 }),
  );
  expect(patched.status).toBe(200);
  expect(patched.json?.id).toBe(id);

  const deleted = await send("DELETE", `/api/v1/orgs/acme/models/${id}`);
  expect(deleted.status).toBe(204);
});
