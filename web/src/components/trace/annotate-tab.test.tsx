import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import type { UserEvent } from "@testing-library/user-event";
import { HttpResponse, http } from "msw";
import { describe, expect, it } from "vitest";
import { server } from "@/mocks/node";
import { BOB_ID } from "@/test/fixtures";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import { serveAnnotationDefinitions } from "@/test/score-definitions";
import { ADA_SCORE_ID, scoreFor, serveScores } from "@/test/scores";
import projectFixture from "../../../../fixtures/api/static/project.json";

const TRACE = "a1000000000000000000000000000001";
const SPAN = "b100000000000014";
const URL = `/acme/support-bot/traces?trace=${TRACE}&span=${SPAN}`;
const BOB_SCORE_ID = "01900000-0000-7000-8000-000000000311";

const ADA_SCORE = scoreFor({});
const BOB_SCORE = scoreFor({
  score_id: BOB_SCORE_ID,
  author_user_id: BOB_ID,
  value: { data_type: "numeric", value: 0.2 },
});

/** Records every score write body and answers with `answer()`. */
function captureScores(
  answer: () => Response = () => HttpResponse.json({ score_ids: [ADA_SCORE_ID] }),
) {
  const bodies: unknown[] = [];
  server.use(
    http.post("/api/v1/projects/:project/scores", async ({ request }) => {
      bodies.push(await request.json());
      return answer();
    }),
  );
  return bodies;
}

/**
 * The trace split pane (react-resizable-panels) cancels pointerdown in jsdom, so user.click never
 * focuses a field and user.type loses its keys. Focus the field first, then type.
 * jsdom has no geometry, so react-resizable-panels treats every pointerdown as a separator hit;
 * in a real browser only the separator's hit margin is affected.
 */
async function typeIn(user: UserEvent, field: HTMLElement, text: string) {
  field.focus();
  await user.keyboard(text);
}

async function setValue(user: UserEvent, field: HTMLElement, text: string) {
  field.focus();
  await user.clear(field);
  await user.keyboard(text);
}

async function openAnnotate() {
  const view = await renderApp(URL);
  const panel = await screen.findByRole("region", { name: "Span" });
  fireEvent.mouseDown(within(panel).getByRole("tab", { name: "Annotate" }));
  const form = await within(panel).findByRole("form", { name: "Annotations" });
  return { ...view, panel, form };
}

describe("annotate tab", () => {
  it("shows one control per live definition", async () => {
    serveAnnotationDefinitions();
    serveScores([]);
    const { panel, form } = await openAnnotate();

    const helpfulness = within(form).getByRole("spinbutton", { name: "helpfulness" });
    expect(helpfulness).toBeInTheDocument();
    expect(within(form).getByText("How helpful the answer is, from 0 to 1.")).toBeInTheDocument();
    expect(within(form).getByText("0 – 1")).toBeInTheDocument();
    expect(helpfulness).toHaveAccessibleDescription(
      "How helpful the answer is, from 0 to 1. 0 – 1",
    );
    const resolved = within(form).getByRole("radiogroup", { name: "resolved" });
    expect(within(resolved).getByRole("radio", { name: "Yes" })).toBeInTheDocument();
    expect(within(resolved).getByRole("radio", { name: "No" })).toBeInTheDocument();
    const tone = within(form).getByRole("radiogroup", { name: "tone" });
    expect(within(tone).getAllByRole("radio")).toHaveLength(3);
    expect(within(form).getByRole("textbox", { name: "note" })).toBeInTheDocument();
    expect(within(form).getByRole("button", { name: "Save" })).toBeDisabled();
    expect(within(panel).getByRole("radio", { name: "Trace" })).toBeChecked();
  });

  it("prefills the caller's own score only", async () => {
    serveAnnotationDefinitions();
    serveScores([ADA_SCORE, BOB_SCORE]);
    const { form } = await openAnnotate();

    expect(within(form).getByRole("spinbutton", { name: "helpfulness" })).toHaveValue(0.7);
    expect(within(form).getByRole("button", { name: "Clear helpfulness" })).toBeInTheDocument();
    expect(within(form).queryByRole("button", { name: "Clear tone" })).not.toBeInTheDocument();
  });

  it("saves the changed rows of the trace without a score id", async () => {
    serveAnnotationDefinitions();
    const reads = serveScores([ADA_SCORE, BOB_SCORE]);
    const bodies = captureScores();
    const { user, form } = await openAnnotate();
    const before = reads.count;

    const helpfulness = within(form).getByRole("spinbutton", { name: "helpfulness" });
    await setValue(user, helpfulness, "0.8");
    await typeIn(
      user,
      within(form).getByRole("textbox", { name: "helpfulness reason" }),
      "clear answer",
    );
    await user.click(within(form).getByRole("radio", { name: "neutral" }));
    await user.click(within(form).getByRole("button", { name: "Save" }));

    expect(await within(form).findByText("Saved.")).toBeInTheDocument();
    expect(bodies).toEqual([
      {
        scores: [
          {
            target: { type: "trace", trace_id: TRACE },
            session_id: "sess-001",
            user_id: "user-42",
            name: "helpfulness",
            value: { data_type: "numeric", value: 0.8 },
            reason: "clear answer",
          },
          {
            target: { type: "trace", trace_id: TRACE },
            session_id: "sess-001",
            user_id: "user-42",
            name: "tone",
            value: { data_type: "categorical", value: { label: "neutral", value: 0.5 } },
          },
        ],
      },
    ]);
    expect(reads.count).toBeGreaterThan(before);
  });

  it("annotates the selected span", async () => {
    serveAnnotationDefinitions();
    serveScores([]);
    const bodies = captureScores();
    const { user, panel } = await openAnnotate();

    await user.click(within(panel).getByRole("radio", { name: "Span" }));
    const spanForm = await within(panel).findByRole("form", { name: "Annotations" });
    await user.click(within(spanForm).getByRole("radio", { name: "Yes" }));
    await user.click(within(spanForm).getByRole("button", { name: "Save" }));

    expect(await within(spanForm).findByText("Saved.")).toBeInTheDocument();
    expect(bodies).toEqual([
      {
        scores: [
          {
            target: { type: "span", trace_id: TRACE, span_id: SPAN },
            session_id: "sess-001",
            user_id: "user-42",
            name: "resolved",
            value: { data_type: "boolean", value: true },
          },
        ],
      },
    ]);
  });

  it("refuses an out-of-range number", async () => {
    serveAnnotationDefinitions();
    serveScores([]);
    const bodies = captureScores();
    const { user, form } = await openAnnotate();

    const helpfulness = within(form).getByRole("spinbutton", { name: "helpfulness" });
    await setValue(user, helpfulness, "2");

    expect(await within(form).findByText("Enter a number from 0 to 1.")).toBeInTheDocument();
    expect(helpfulness).toHaveAttribute("aria-invalid", "true");
    expect(helpfulness).toHaveAccessibleDescription(
      "How helpful the answer is, from 0 to 1. 0 – 1 Enter a number from 0 to 1.",
    );
    expect(within(form).getByRole("button", { name: "Save" })).toBeDisabled();
    expect(bodies).toEqual([]);
  });

  it("does not flag an unchanged prefilled score that its definition now rejects", async () => {
    serveAnnotationDefinitions();
    serveScores([scoreFor({ value: { data_type: "numeric", value: 1.5 } })]);
    const { form } = await openAnnotate();

    const helpfulness = within(form).getByRole("spinbutton", { name: "helpfulness" });
    expect(helpfulness).toHaveValue(1.5);
    expect(helpfulness).toHaveAttribute("aria-invalid", "false");
    expect(within(form).queryByText("Enter a number from 0 to 1.")).not.toBeInTheDocument();
    expect(within(form).getByRole("button", { name: "Save" })).toBeDisabled();
  });

  it("says what a row needs before it saves", async () => {
    serveAnnotationDefinitions();
    serveScores([ADA_SCORE]);
    const { user, form } = await openAnnotate();

    await typeIn(user, within(form).getByRole("textbox", { name: "helpfulness reason" }), "why");
    expect(within(form).queryByText("Use Clear to remove your score.")).not.toBeInTheDocument();
    expect(within(form).getByRole("button", { name: "Save" })).toBeEnabled();

    const helpfulness = within(form).getByRole("spinbutton", { name: "helpfulness" });
    helpfulness.focus();
    await user.clear(helpfulness);
    expect(within(form).getByText("Use Clear to remove your score.")).toBeInTheDocument();
    expect(within(form).getByRole("button", { name: "Save" })).toBeDisabled();
  });

  it("asks for a value on a new row with only a reason", async () => {
    serveAnnotationDefinitions();
    serveScores([]);
    const { user, form } = await openAnnotate();

    await typeIn(user, within(form).getByRole("textbox", { name: "helpfulness reason" }), "why");
    expect(within(form).getByText("Set a value to save this row.")).toBeInTheDocument();
    expect(within(form).queryByText(/Enter a number/)).not.toBeInTheDocument();
  });

  it("keeps the fields focusable while a save is in flight", async () => {
    serveAnnotationDefinitions();
    serveScores([]);
    let release: () => void = () => {};
    const gate = new Promise<void>((resolve) => {
      release = resolve;
    });
    server.use(
      http.post("/api/v1/projects/:project/scores", async () => {
        await gate;
        return HttpResponse.json({ score_ids: [ADA_SCORE_ID] });
      }),
    );
    const { user, form } = await openAnnotate();

    await setValue(user, within(form).getByRole("spinbutton", { name: "helpfulness" }), "0.5");
    await user.click(within(form).getByRole("button", { name: "Save" }));
    await waitFor(() => {
      expect(within(form).getByRole("button", { name: "Save" })).toBeDisabled();
    });

    const note = within(form).getByRole("textbox", { name: "note" });
    await typeIn(user, note, "typed while saving");
    expect(note).toHaveFocus();
    expect(note).not.toBeDisabled();
    expect(note).toHaveAttribute("aria-disabled", "true");
    expect(note).toHaveValue("");

    release();
    expect(await within(form).findByText("Saved.")).toBeInTheDocument();
  });

  it("keeps the score when a clear fails", async () => {
    serveAnnotationDefinitions();
    serveScores([ADA_SCORE, BOB_SCORE]);
    server.use(
      http.delete("/api/v1/projects/:project/scores/:score_id", () =>
        problemResponse(500, "internal", { detail: "The score store is unavailable." }),
      ),
    );
    const { user, form } = await openAnnotate();

    await user.click(within(form).getByRole("button", { name: "Clear helpfulness" }));

    expect(await within(form).findByRole("alert")).toHaveTextContent(
      "The score store is unavailable.",
    );
    expect(within(form).getByRole("spinbutton", { name: "helpfulness" })).toHaveValue(0.7);
    expect(within(form).getByRole("button", { name: "Clear helpfulness" })).toBeInTheDocument();
  });

  it("shows a save the server refuses for the caller's role", async () => {
    serveAnnotationDefinitions();
    serveScores([]);
    captureScores(() =>
      problemResponse(403, "forbidden", { detail: "Your role cannot write scores." }),
    );
    const { user, form } = await openAnnotate();

    await setValue(user, within(form).getByRole("spinbutton", { name: "helpfulness" }), "0.5");
    await user.click(within(form).getByRole("button", { name: "Save" }));

    expect(await within(form).findByRole("alert")).toHaveTextContent(
      "Your role cannot write scores.",
    );
  });

  it("shows a server error on its row", async () => {
    serveAnnotationDefinitions();
    serveScores([]);
    captureScores(() =>
      problemResponse(400, "bad_request", {
        detail: "invalid scores",
        errors: [
          { field: "scores[0].value", message: "value is outside the score definition's range" },
        ],
      }),
    );
    const { user, form } = await openAnnotate();

    const helpfulness = within(form).getByRole("spinbutton", { name: "helpfulness" });
    await setValue(user, helpfulness, "0.5");
    await user.click(within(form).getByRole("button", { name: "Save" }));

    const group = within(form).getByRole("group", { name: "helpfulness" });
    expect(
      await within(group).findByText("value is outside the score definition's range"),
    ).toBeInTheDocument();
  });

  it("shows the units cap", async () => {
    serveAnnotationDefinitions();
    serveScores([]);
    captureScores(() =>
      problemResponse(402, "quota_exceeded", {
        detail: "Monthly units cap reached; resets on 2026-02-01.",
      }),
    );
    const { user, form } = await openAnnotate();

    const helpfulness = within(form).getByRole("spinbutton", { name: "helpfulness" });
    await setValue(user, helpfulness, "0.5");
    await user.click(within(form).getByRole("button", { name: "Save" }));

    expect(await within(form).findByRole("alert")).toHaveTextContent(
      "Monthly units cap reached; resets on 2026-02-01.",
    );
  });

  it("clears the caller's own score", async () => {
    serveAnnotationDefinitions();
    serveScores([ADA_SCORE, BOB_SCORE]);
    const deleted: string[] = [];
    server.use(
      http.delete("/api/v1/projects/:project/scores/:score_id", ({ params }) => {
        deleted.push(String(params.score_id));
        return new HttpResponse(null, { status: 204 });
      }),
    );
    const { user, form } = await openAnnotate();

    await user.click(within(form).getByRole("button", { name: "Clear helpfulness" }));

    await waitFor(() => {
      expect(deleted).toEqual([ADA_SCORE_ID]);
    });
  });

  it("does not let a viewer annotate", async () => {
    serveAnnotationDefinitions();
    serveScores([]);
    server.use(
      http.get("/api/v1/projects/:project", () =>
        HttpResponse.json({ ...projectFixture, role: "viewer" }),
      ),
    );
    await renderApp(URL);
    const panel = await screen.findByRole("region", { name: "Span" });
    fireEvent.mouseDown(within(panel).getByRole("tab", { name: "Annotate" }));

    expect(await within(panel).findByText("Only members can annotate.")).toBeInTheDocument();
    expect(within(panel).queryByRole("button", { name: "Save" })).not.toBeInTheDocument();
  });

  it("says when there are no score definitions", async () => {
    serveAnnotationDefinitions([]);
    serveScores([]);
    await renderApp(URL);
    const panel = await screen.findByRole("region", { name: "Span" });
    fireEvent.mouseDown(within(panel).getByRole("tab", { name: "Annotate" }));

    expect(await within(panel).findByText("No score definitions yet.")).toBeInTheDocument();
    expect(within(panel).getByRole("link", { name: "Add score definitions" })).toHaveAttribute(
      "href",
      "/acme/support-bot/settings/score-definitions",
    );
  });
});
