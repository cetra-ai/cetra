import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { act, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { describe, expect, it, vi } from "vitest";
import type { Schema } from "@/api/types";
import { AnnotationForm } from "@/components/annotation/annotation-form";
import { ProjectProvider } from "@/components/shell/project-context";
import type { AnnotationSaveHandle, AnnotationTarget } from "@/lib/annotation";
import { server } from "@/mocks/node";
import { ADA_ID } from "@/test/fixtures";
import { problemResponse } from "@/test/problem";
import { serveAnnotationDefinitions } from "@/test/score-definitions";
import { ADA_SCORE_ID, SCORE_TRACE, scoreFor, serveScores } from "@/test/scores";
import projectFixture from "../../../../fixtures/api/static/project.json";

const TARGET: AnnotationTarget = { type: "trace", traceId: SCORE_TRACE };
const QUEUE = "01950000-0000-7000-8000-000000000a01";
const HELPFULNESS = "01900000-0000-7000-8000-000000000601";
const TONE = "01900000-0000-7000-8000-000000000603";

/** Records every score write body. The answer is `{score_ids}` unless `answer` is given. */
function captureScores(answer: () => Response = () => HttpResponse.json({ score_ids: [] })) {
  const bodies: unknown[] = [];
  server.use(
    http.post("/api/v1/projects/:project/scores", async ({ request }) => {
      bodies.push(await request.json());
      return answer();
    }),
  );
  return bodies;
}

function renderForm(
  props: Partial<Parameters<typeof AnnotationForm>[0]> = {},
): { saveRef: { current: AnnotationSaveHandle | null }; onReady: ReturnType<typeof vi.fn> } {
  const queryClient = new QueryClient({
    defaultOptions: { queries: { retry: false }, mutations: { retry: false } },
  });
  const saveRef: { current: AnnotationSaveHandle | null } = { current: null };
  const onReady = vi.fn();
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
        <AnnotationForm
          target={TARGET}
          context={{ userId: "user-42", queueId: QUEUE }}
          saveRef={saveRef}
          onReady={onReady}
          {...props}
        />
      </ProjectProvider>
    </QueryClientProvider>,
  );
  return { saveRef, onReady };
}

/** The save handle, once the rows are shown. */
function handleOf(saveRef: { current: AnnotationSaveHandle | null }): AnnotationSaveHandle {
  const handle = saveRef.current;
  if (handle === null) {
    throw new Error("the save handle is not set");
  }
  return handle;
}

describe("annotation form", () => {
  it("restricts and orders the controls by definitionIds", async () => {
    serveAnnotationDefinitions();
    serveScores([]);
    renderForm({ definitionIds: [TONE, HELPFULNESS] });

    const tone = await screen.findByRole("radiogroup", { name: "tone" });
    const helpfulness = screen.getByRole("spinbutton", { name: "helpfulness" });
    // DOCUMENT_POSITION_FOLLOWING: the helpfulness row comes after the tone row.
    expect(
      tone.compareDocumentPosition(helpfulness) & Node.DOCUMENT_POSITION_FOLLOWING,
    ).toBeTruthy();
    expect(screen.queryByRole("radiogroup", { name: "resolved" })).not.toBeInTheDocument();
    expect(screen.queryByRole("textbox", { name: "note" })).not.toBeInTheDocument();
  });

  it("shows the notice when every queue definition is archived", async () => {
    serveAnnotationDefinitions([]);
    serveScores([]);
    renderForm({ definitionIds: [HELPFULNESS] });

    expect(
      await screen.findByText("This queue's score definitions are all archived."),
    ).toBeInTheDocument();
    expect(screen.queryByRole("form", { name: "Annotations" })).not.toBeInTheDocument();
  });

  it("sends the queue id and no score id through the save handle", async () => {
    serveAnnotationDefinitions();
    serveScores([]);
    const bodies = captureScores(() => HttpResponse.json({ score_ids: [ADA_SCORE_ID] }));
    const { saveRef } = renderForm({ definitionIds: [HELPFULNESS] });
    const helpfulness = await screen.findByRole("spinbutton", { name: "helpfulness" });

    fireEvent.change(helpfulness, { target: { value: "0.8" } });
    expect(handleOf(saveRef).isDirty()).toBe(true);
    let saved = false;
    await act(async () => {
      saved = await handleOf(saveRef).save();
    });

    expect(saved).toBe(true);
    expect(bodies).toHaveLength(1);
    const item = (bodies[0] as { scores: Record<string, unknown>[] }).scores[0];
    expect(item).toMatchObject({ queue_id: QUEUE, name: "helpfulness" });
    expect("score_id" in (item ?? {})).toBe(false);
  });

  it("saves nothing and resolves true when no row changed", async () => {
    serveAnnotationDefinitions();
    serveScores([]);
    const bodies = captureScores();
    const { saveRef } = renderForm({ definitionIds: [HELPFULNESS] });
    await screen.findByRole("spinbutton", { name: "helpfulness" });

    expect(handleOf(saveRef).isDirty()).toBe(false);
    let saved = false;
    await act(async () => {
      saved = await handleOf(saveRef).save();
    });

    expect(saved).toBe(true);
    expect(bodies).toHaveLength(0);
  });

  it("resolves false and shows the error when the save fails", async () => {
    serveAnnotationDefinitions();
    serveScores([]);
    captureScores(() =>
      problemResponse(402, "quota_exceeded", {
        detail: "Monthly units cap reached; resets on 2026-02-01.",
      }),
    );
    const { saveRef } = renderForm({ definitionIds: [HELPFULNESS] });
    const helpfulness = await screen.findByRole("spinbutton", { name: "helpfulness" });

    fireEvent.change(helpfulness, { target: { value: "0.8" } });
    let saved = true;
    await act(async () => {
      saved = await handleOf(saveRef).save();
    });

    expect(saved).toBe(false);
    expect(await screen.findByRole("alert")).toHaveTextContent(
      "Monthly units cap reached; resets on 2026-02-01.",
    );
  });

  it("calls onReady once after loading", async () => {
    serveAnnotationDefinitions();
    serveScores([scoreFor({ author_user_id: ADA_ID })]);
    const { onReady } = renderForm({ definitionIds: [HELPFULNESS] });

    await screen.findByRole("spinbutton", { name: "helpfulness" });
    await waitFor(() => {
      expect(onReady).toHaveBeenCalled();
    });
    expect(onReady).toHaveBeenCalledTimes(1);
  });
});
