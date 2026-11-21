import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { expect, it, vi } from "vitest";
import type { Schema } from "@/api/types";
import { QueueDialog } from "@/components/annotation-queues/queue-dialog";
import { ProjectProvider } from "@/components/shell/project-context";
import { QUEUE_NAME_TAKEN } from "@/lib/annotation-queues";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import { serveAnnotationDefinitions } from "@/test/score-definitions";
import queueFixture from "../../../../fixtures/api/static/annotation-queue.json";
import entitlementsFixture from "../../../../fixtures/api/static/org-entitlements.json";
import projectFixture from "../../../../fixtures/api/static/project.json";

type Queue = Schema<"AnnotationQueue">;

const QUEUE = queueFixture as unknown as Queue;
const HELPFULNESS = "01900000-0000-7000-8000-000000000601";
const TONE = "01900000-0000-7000-8000-000000000603";

const QUEUES = "/api/v1/projects/:project/annotation-queues";
const QUEUE_PATH = "/api/v1/projects/:project/annotation-queues/:id";

/** Records every body sent to `path` with `method` and answers with `answer()`. */
function captureWrites(method: "post" | "patch", path: string, answer: () => Response) {
  const bodies: unknown[] = [];
  const handler = async ({ request }: { request: Request }) => {
    bodies.push(await request.json());
    return answer();
  };
  server.use(method === "post" ? http.post(path, handler) : http.patch(path, handler));
  return bodies;
}

function serveEntitlements() {
  server.use(
    http.get("/api/v1/orgs/:org/entitlements", () => HttpResponse.json(entitlementsFixture)),
  );
}

function renderDialog(queue?: Queue) {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  const onCreated = vi.fn();
  const onOpenChange = vi.fn();
  render(
    <QueryClientProvider client={queryClient}>
      <ProjectProvider
        value={{
          org: "acme",
          project: "support-bot",
          ref: "acme/support-bot",
          data: projectFixture as Schema<"Project">,
        }}
      >
        <QueueDialog open onOpenChange={onOpenChange} queue={queue} onCreated={onCreated} />
      </ProjectProvider>
    </QueryClientProvider>,
  );
  return { onCreated, onOpenChange };
}

it("creates a queue with its definitions in order and its instructions", async () => {
  const user = userEvent.setup();
  serveAnnotationDefinitions();
  serveEntitlements();
  const bodies = captureWrites("post", QUEUES, () => HttpResponse.json(QUEUE, { status: 201 }));
  const { onCreated } = renderDialog();

  await user.type(await screen.findByLabelText("Name"), "Relevance review");
  await user.click(await screen.findByRole("checkbox", { name: "helpfulness" }));
  await user.click(screen.getByRole("checkbox", { name: "tone" }));
  await user.type(screen.getByLabelText("Instructions"), "Score each answer.");
  await user.click(screen.getByRole("button", { name: "Create queue" }));

  expect(bodies).toEqual([
    {
      name: "Relevance review",
      target: "trace",
      definition_ids: [HELPFULNESS, TONE],
      instructions: "Score each answer.",
    },
  ]);
  expect(onCreated).toHaveBeenCalledWith(expect.objectContaining({ id: QUEUE.id }));
});

it("shows a taken name on the name field", async () => {
  const user = userEvent.setup();
  serveAnnotationDefinitions();
  serveEntitlements();
  captureWrites("post", QUEUES, () =>
    problemResponse(409, "conflict", { detail: QUEUE_NAME_TAKEN, title: "Conflict" }),
  );
  renderDialog();

  await user.type(await screen.findByLabelText("Name"), "Relevance review");
  await user.click(await screen.findByRole("checkbox", { name: "helpfulness" }));
  await user.click(screen.getByRole("button", { name: "Create queue" }));

  expect(await screen.findByText("A queue with this name exists.")).toBeInTheDocument();
});

it("shows the plan limit with the upgrade link", async () => {
  const user = userEvent.setup();
  serveAnnotationDefinitions();
  serveEntitlements();
  captureWrites("post", QUEUES, () =>
    problemResponse(403, "plan_limit", {
      detail: "The free plan allows one annotation queue.",
    }),
  );
  renderDialog();

  await user.type(await screen.findByLabelText("Name"), "Relevance review");
  await user.click(await screen.findByRole("checkbox", { name: "helpfulness" }));
  await user.click(screen.getByRole("button", { name: "Create queue" }));

  expect(await screen.findByRole("alert")).toHaveTextContent(
    "The free plan allows one annotation queue.",
  );
  expect(await screen.findByRole("link", { name: "Upgrade" })).toHaveAttribute(
    "href",
    entitlementsFixture.upgrade_url,
  );
});

it("sends only the changed fields with the version", async () => {
  const user = userEvent.setup();
  serveAnnotationDefinitions();
  serveEntitlements();
  const bodies = captureWrites("patch", QUEUE_PATH, () => HttpResponse.json(QUEUE));
  renderDialog(QUEUE);

  const name = await screen.findByLabelText("Name");
  await user.clear(name);
  await user.type(name, "Relevance v2");
  await user.click(screen.getByRole("button", { name: "Save" }));

  expect(bodies).toEqual([{ version: QUEUE.version, name: "Relevance v2" }]);
});

it("tells the user when someone else changed the queue", async () => {
  const user = userEvent.setup();
  serveAnnotationDefinitions();
  serveEntitlements();
  captureWrites("patch", QUEUE_PATH, () =>
    problemResponse(409, "conflict", {
      detail: "the annotation queue changed; current version is 3",
    }),
  );
  renderDialog(QUEUE);

  const name = await screen.findByLabelText("Name");
  await user.clear(name);
  await user.type(name, "Relevance v2");
  await user.click(screen.getByRole("button", { name: "Save" }));

  expect(await screen.findByRole("alert")).toHaveTextContent(
    "Someone else changed this queue. Close and open it again.",
  );
});
