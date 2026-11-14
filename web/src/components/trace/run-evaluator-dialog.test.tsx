import { screen, waitFor, within } from "@testing-library/react";
import type { UserEvent } from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { describe, expect, it, vi } from "vitest";
import { ERROR_HINTS } from "@/components/evaluators/error-hints";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import runFixture from "../../../../fixtures/api/requests/evaluator-run.json";
import queuedFixture from "../../../../fixtures/api/static/eval-job-queued.json";
import projectFixture from "../../../../fixtures/api/static/project.json";

const TRACE = "a1000000000000000000000000000001";
const ROOT = "b100000000000011";
const SPAN = "b100000000000014";
const JUDGE = "e1000000000000000000000000000002";
const JOB = "01900000-0000-7000-8000-000000000802";
const RELEVANCE_OPTION = "Relevance (LLM judge)";

const DONE = {
  ...queuedFixture,
  status: "done",
  attempts: 1,
  judge_trace_id: JUDGE,
  score_id: "01900000-0000-7000-8000-000000000900",
  finished_at: "2026-01-01T10:10:01.000000000Z",
};

/** Answers each run request with `answer` and records the bodies it receives. */
function captureRuns(answer: () => Response): unknown[] {
  const bodies: unknown[] = [];
  server.use(
    http.post("/api/v1/projects/:project/evaluators/:id/run", async ({ request }) => {
      bodies.push(await request.json());
      return answer();
    }),
  );
  return bodies;
}

/** Answers each job read with the next body; the last one repeats. */
function pollJob(...bodies: object[]) {
  let calls = 0;
  server.use(
    http.get("/api/v1/projects/:project/eval-jobs/:id", () => {
      const body = bodies[Math.min(calls, bodies.length - 1)];
      calls += 1;
      return HttpResponse.json(body);
    }),
  );
}

/** Opens the trace page and its "Run evaluator" dialog. */
async function openDialog(url: string) {
  const rendered = await renderApp(url);
  const header = await screen.findByRole("region", { name: "Trace" });
  await rendered.user.click(within(header).getByRole("button", { name: "Run evaluator" }));
  return { ...rendered, dialog: await screen.findByRole("dialog") };
}

/** Picks an option of the Radix select named `name`, inside the dialog. */
async function choose(user: UserEvent, dialog: HTMLElement, name: string, option: string) {
  await user.click(within(dialog).getByRole("combobox", { name }));
  await user.click(await screen.findByRole("option", { name: option }));
}

/** The variable mapping rows of the dialog, as `[variable, expression]` pairs. */
function mappingRows(dialog: HTMLElement): [string, string][] {
  return within(dialog)
    .queryAllByLabelText(/^Mapping variable \d+$/)
    .map((variable, index): [string, string] => [
      (variable as HTMLInputElement).value,
      (within(dialog).getByLabelText(`Mapping expression ${index + 1}`) as HTMLInputElement).value,
    ]);
}

describe("run evaluator dialog", () => {
  it("runs on the whole trace and shows the result", async () => {
    const bodies = captureRuns(() => HttpResponse.json(queuedFixture, { status: 202 }));
    pollJob(queuedFixture, DONE);
    const { user, dialog } = await openDialog(
      `/acme/support-bot/traces?mock=fixtures&trace=${TRACE}`,
    );

    await choose(user, dialog, "Evaluator", RELEVANCE_OPTION);
    expect(mappingRows(dialog)).toEqual([
      ["input", "trace.input"],
      ["output", "trace.output"],
    ]);
    await user.click(within(dialog).getByRole("button", { name: "Run" }));

    await waitFor(() => expect(bodies).toEqual([runFixture]));
    expect(
      await within(dialog).findByText("Score written", {}, { timeout: 3000 }),
    ).toBeInTheDocument();
    expect(within(dialog).getByRole("link", { name: "View judge trace" })).toHaveAttribute(
      "href",
      `/acme/support-bot/traces/${JUDGE}`,
    );
  });

  it("runs on the selected span with span expressions", async () => {
    const bodies = captureRuns(() => HttpResponse.json(queuedFixture, { status: 202 }));
    const { user, dialog } = await openDialog(
      `/acme/support-bot/traces?mock=fixtures&trace=${TRACE}&span=${SPAN}`,
    );

    await choose(user, dialog, "Evaluator", RELEVANCE_OPTION);
    await user.click(within(dialog).getByRole("radio", { name: "Selected span" }));
    expect(mappingRows(dialog)).toEqual([
      ["input", "span.input"],
      ["output", "span.output"],
    ]);
    await user.click(within(dialog).getByRole("button", { name: "Run" }));

    await waitFor(() =>
      expect(bodies).toEqual([
        {
          ...runFixture,
          span_id: SPAN,
          variable_mapping: { input: "span.input", output: "span.output" },
        },
      ]),
    );
  });

  it("offers only the whole trace for a root span", async () => {
    const { dialog } = await openDialog(
      `/acme/support-bot/traces?mock=fixtures&trace=${TRACE}&span=${ROOT}`,
    );

    expect(within(dialog).queryByRole("radio", { name: "Selected span" })).toBeNull();
  });

  it("shows a failed run with its hint", async () => {
    captureRuns(() => HttpResponse.json(queuedFixture, { status: 202 }));
    pollJob({
      ...queuedFixture,
      status: "failed",
      attempts: 6,
      error_class: "retryable",
      error: "provider status 503",
      finished_at: "2026-01-01T10:10:01.000000000Z",
    });
    const { user, dialog } = await openDialog(
      `/acme/support-bot/traces?mock=fixtures&trace=${TRACE}`,
    );

    await choose(user, dialog, "Evaluator", RELEVANCE_OPTION);
    await user.click(within(dialog).getByRole("button", { name: "Run" }));

    expect(
      await within(dialog).findByText("Run failed", {}, { timeout: 3000 }),
    ).toBeInTheDocument();
    expect(within(dialog).getByText("provider status 503")).toBeInTheDocument();
    expect(within(dialog).getByText(ERROR_HINTS.retryable)).toBeInTheDocument();
  });

  it("shows the wait message when the rate limit answers", async () => {
    server.use(
      http.post("/api/v1/projects/:project/evaluators/:id/run", () =>
        problemResponse(429, "rate_limited"),
      ),
    );
    const { user, dialog } = await openDialog(
      `/acme/support-bot/traces?mock=fixtures&trace=${TRACE}`,
    );

    await choose(user, dialog, "Evaluator", RELEVANCE_OPTION);
    await user.click(within(dialog).getByRole("button", { name: "Run" }));

    expect(await within(dialog).findByText("Too many runs. Wait a minute.")).toBeInTheDocument();
  });

  it("does not let a viewer run", async () => {
    server.use(
      http.get("/api/v1/projects/:project", () =>
        HttpResponse.json({ ...projectFixture, role: "viewer" }),
      ),
    );
    await renderApp(`/acme/support-bot/traces?mock=fixtures&trace=${TRACE}`);
    const header = await screen.findByRole("region", { name: "Trace" });

    expect(within(header).getByRole("button", { name: "Run evaluator" })).toBeDisabled();
  });

  it("says when the run's status cannot be read", async () => {
    captureRuns(() => HttpResponse.json(queuedFixture, { status: 202 }));
    server.use(
      http.get("/api/v1/projects/:project/eval-jobs/:id", () => problemResponse(500, "internal")),
    );
    const { user, dialog } = await openDialog(
      `/acme/support-bot/traces?mock=fixtures&trace=${TRACE}`,
    );

    await choose(user, dialog, "Evaluator", RELEVANCE_OPTION);
    await user.click(within(dialog).getByRole("button", { name: "Run" }));

    const alert = await within(dialog).findByRole("alert");
    expect(alert).toHaveTextContent("Could not read the run's status.");
    expect(within(alert).getByRole("link", { name: "View jobs" })).toHaveAttribute(
      "href",
      "/acme/support-bot/online-rules",
    );
    expect(within(dialog).queryByText("Queued")).toBeNull();
  });

  it("disables Run while the job is queued", async () => {
    captureRuns(() => HttpResponse.json(queuedFixture, { status: 202 }));
    pollJob(queuedFixture);
    const { user, dialog } = await openDialog(
      `/acme/support-bot/traces?mock=fixtures&trace=${TRACE}`,
    );

    await choose(user, dialog, "Evaluator", RELEVANCE_OPTION);
    await user.click(within(dialog).getByRole("button", { name: "Run" }));

    expect(await within(dialog).findByText("Queued")).toBeInTheDocument();
    expect(within(dialog).getByRole("button", { name: "Run" })).toBeDisabled();
  });

  it("refreshes the scores when a run finishes after the dialog closed", async () => {
    captureRuns(() => HttpResponse.json(queuedFixture, { status: 202 }));
    pollJob(queuedFixture, DONE);
    const { user, dialog, queryClient } = await openDialog(
      `/acme/support-bot/traces?mock=fixtures&trace=${TRACE}`,
    );
    const invalidate = vi.spyOn(queryClient, "invalidateQueries");

    await choose(user, dialog, "Evaluator", RELEVANCE_OPTION);
    await user.click(within(dialog).getByRole("button", { name: "Run" }));
    expect(await within(dialog).findByText("Queued")).toBeInTheDocument();
    await user.keyboard("{Escape}");
    await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
    expect(invalidate).not.toHaveBeenCalled();

    await waitFor(
      () =>
        expect(invalidate).toHaveBeenCalledWith({
          queryKey: ["get", "/api/v1/projects/{project}/scores"],
        }),
      { timeout: 3000 },
    );
  });

  it("says when the evaluators cannot be loaded", async () => {
    server.use(
      http.get("/api/v1/projects/:project/evaluators", () => problemResponse(500, "internal")),
    );
    const { dialog } = await openDialog(`/acme/support-bot/traces?mock=fixtures&trace=${TRACE}`);

    expect(await within(dialog).findByRole("alert")).toHaveTextContent(
      "Could not load the evaluators.",
    );
    expect(within(dialog).getByRole("button", { name: "Run" })).toBeDisabled();
  });
});
