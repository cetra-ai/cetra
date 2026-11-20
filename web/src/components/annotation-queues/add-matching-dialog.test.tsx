import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { describe, expect, it, vi } from "vitest";
import type { Schema } from "@/api/types";
import {
  AddMatchingDialog,
  type MatchingSelection,
} from "@/components/annotation-queues/add-matching-dialog";
import { ProjectProvider } from "@/components/shell/project-context";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import projectFixture from "../../../../fixtures/api/static/project.json";
import queuesFixture from "../../../../fixtures/api/static/annotation-queues.json";

type Queue = Schema<"AnnotationQueue">;
const QUEUE: Queue = queuesFixture.data[0] as Queue;
const SESSION_QUEUE: Queue = {
  ...QUEUE,
  id: "01900000-0000-7000-8000-000000000a03",
  name: "Session review",
  target: "session",
};

const LIST = "/api/v1/projects/:project/annotation-queues";
const PREVIEW = "/api/v1/projects/:project/annotation-queues/:id/items/matching/preview";
const ADD = "/api/v1/projects/:project/annotation-queues/:id/items/matching";

const SELECTION: MatchingSelection = {
  filter: "name = 'x'",
  search: "refund",
  from: "2026-01-01T00:00:00Z",
  to: "2026-01-02T00:00:00Z",
  includeExperiments: false,
};

/** Serves these queues and records the body and queue id of each preview and add. */
function serve(queues: Queue[]) {
  const previews: { id: string; body: unknown }[] = [];
  const adds: { id: string; body: unknown }[] = [];
  server.use(
    http.get(LIST, () => HttpResponse.json({ data: queues, next_cursor: null })),
    http.post(PREVIEW, async ({ request, params }) => {
      previews.push({ id: String(params.id), body: await request.json() });
      return HttpResponse.json({ matched: 42, capped: false });
    }),
    http.post(ADD, async ({ request, params }) => {
      adds.push({ id: String(params.id), body: await request.json() });
      const answer = { added: 40, existing: 2, blocked: 0, capped: false };
      return HttpResponse.json(answer, { status: 201 });
    }),
  );
  return { previews, adds };
}

function renderDialog(props: Partial<Parameters<typeof AddMatchingDialog>[0]> = {}) {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
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
        <AddMatchingDialog
          open
          onOpenChange={vi.fn()}
          target="trace"
          selection={SELECTION}
          {...props}
        />
      </ProjectProvider>
    </QueryClientProvider>,
  );
  return { user: userEvent.setup() };
}

describe("add matching to queue", () => {
  it("counts the matches in the chosen queue, then adds them with the same body", async () => {
    const { previews, adds } = serve([QUEUE]);
    const { user } = renderDialog();
    const dialog = await screen.findByRole("dialog");
    await user.click(within(dialog).getByRole("combobox", { name: "Queue" }));
    await user.click(await screen.findByRole("option", { name: "Relevance review" }));

    expect(await within(dialog).findByText("42 traces match.")).toBeInTheDocument();
    const body = {
      filter: "name = 'x'",
      search: "refund",
      from: SELECTION.from,
      to: SELECTION.to,
      include_experiments: false,
    };
    expect(previews).toEqual([{ id: QUEUE.id, body }]);

    await user.click(within(dialog).getByRole("button", { name: "Add 42 to queue" }));
    expect(
      await within(dialog).findByText("Added 40, 2 already there, 0 blocked by a deletion."),
    ).toBeInTheDocument();
    expect(adds).toEqual([{ id: QUEUE.id, body }]);
  });

  it("never sends a search to a session queue", async () => {
    const { previews } = serve([SESSION_QUEUE]);
    const { user } = renderDialog({ target: "session" });
    const dialog = await screen.findByRole("dialog");
    await user.click(within(dialog).getByRole("combobox", { name: "Queue" }));
    await user.click(await screen.findByRole("option", { name: "Session review" }));

    await within(dialog).findByText("42 sessions match.");
    expect(previews).toHaveLength(1);
    expect(previews[0]?.body).not.toHaveProperty("search");
  });

  it("says that only the first 10 000 are added when the matches are capped", async () => {
    serve([QUEUE]);
    server.use(
      http.post(PREVIEW, () => HttpResponse.json({ matched: 10_000, capped: true })),
    );
    const { user } = renderDialog();
    const dialog = await screen.findByRole("dialog");
    await user.click(within(dialog).getByRole("combobox", { name: "Queue" }));
    await user.click(await screen.findByRole("option", { name: "Relevance review" }));

    expect(
      await within(dialog).findByText(
        "Only the first 10 000 are added. Narrow the filter to add the rest.",
      ),
    ).toBeInTheDocument();
  });

  it("explains an invalid filter", async () => {
    serve([QUEUE]);
    server.use(
      http.post(PREVIEW, () => problemResponse(400, "invalid_filter")),
    );
    const { user } = renderDialog();
    const dialog = await screen.findByRole("dialog");
    await user.click(within(dialog).getByRole("combobox", { name: "Queue" }));
    await user.click(await screen.findByRole("option", { name: "Relevance review" }));

    expect(
      await within(dialog).findByText("The filter is not valid. Fix it on the list first."),
    ).toBeInTheDocument();
  });
});
