import { screen, waitFor } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { problemResponse } from "@/test/problem";
import { renderApp } from "@/test/render";
import previewFixture from "../../../../fixtures/api/static/online-rule-preview.json";

const PREVIEW = "/api/v1/projects/:project/online-rules/preview";
const NEW_RULE = "/acme/support-bot/online-rules/new";
const FIXTURE_TEXT = "1,234 traces matched in the last 24 hours";

type PreviewBody = { target: string; filter?: string };

/** Records the body of every preview request and answers it with `answer`. */
function capturePreviews(
  answer: (body: PreviewBody) => Response = () => HttpResponse.json(previewFixture),
): PreviewBody[] {
  const bodies: PreviewBody[] = [];
  server.use(
    http.post(PREVIEW, async ({ request }) => {
      const body = (await request.json()) as PreviewBody;
      bodies.push(body);
      return answer(body);
    }),
  );
  return bodies;
}

it("shows the match count", async () => {
  capturePreviews();
  await renderApp(NEW_RULE);

  expect(await screen.findByText(FIXTURE_TEXT)).toBeInTheDocument();
  expect(screen.getByRole("status")).toHaveTextContent(FIXTURE_TEXT);
});

it("shows the sampled estimate", async () => {
  capturePreviews();
  const { user } = await renderApp(NEW_RULE);
  await screen.findByText(FIXTURE_TEXT);

  await user.clear(screen.getByLabelText("Sample rate (%)"));
  await user.type(screen.getByLabelText("Sample rate (%)"), "25");

  await waitFor(() =>
    expect(screen.getByRole("status")).toHaveTextContent("about 309 would be evaluated at 25 %"),
  );
});

it("debounces the filter", async () => {
  const bodies = capturePreviews();
  const { user } = await renderApp(NEW_RULE);
  await screen.findByText(FIXTURE_TEXT);

  await user.type(screen.getByLabelText("Filter"), "status = 'error'");

  await waitFor(() =>
    expect(bodies.at(-1)).toEqual({ target: "trace", filter: "status = 'error'" }),
  );
  expect(bodies.map((body) => body.filter).filter((filter) => filter !== undefined)).toEqual([
    "status = 'error'",
  ]);
});

it("says more than 10,000 when capped", async () => {
  capturePreviews(() => HttpResponse.json({ ...previewFixture, matched: 10000, capped: true }));
  await renderApp(NEW_RULE);

  expect(
    await screen.findByText("More than 10,000 traces matched in the last 24 hours"),
  ).toBeInTheDocument();
});

it("counts spans for a span rule", async () => {
  const bodies = capturePreviews((body) =>
    HttpResponse.json({ ...previewFixture, matched: body.target === "span" ? 42 : 1234 }),
  );
  const { user } = await renderApp(NEW_RULE);
  await screen.findByText(FIXTURE_TEXT);

  await user.click(screen.getByRole("radio", { name: "Spans" }));

  expect(await screen.findByText("42 spans matched in the last 24 hours")).toBeInTheDocument();
  expect(bodies.at(-1)).toEqual({ target: "span" });
});

it("shows an invalid filter", async () => {
  capturePreviews((body) =>
    body.filter === undefined
      ? HttpResponse.json(previewFixture)
      : problemResponse(400, "invalid_filter", {
          errors: [{ field: "filter", message: "unknown field status_code" }],
        }),
  );
  const { user } = await renderApp(NEW_RULE);
  await user.type(await screen.findByLabelText("Filter"), "status_code = 1");

  expect(
    await screen.findByText("The filter is invalid: unknown field status_code"),
  ).toBeInTheDocument();
});

it("shows a load error", async () => {
  capturePreviews(() => problemResponse(503, "unavailable"));
  await renderApp(NEW_RULE);

  expect(await screen.findByText("The match count could not be loaded.")).toBeInTheDocument();
});

it("never counts a new target with the filter it replaced", async () => {
  const bodies = capturePreviews();
  const { user } = await renderApp(NEW_RULE);
  await screen.findByText(FIXTURE_TEXT);

  // The target changes before the filter's debounce has run.
  await user.type(screen.getByLabelText("Filter"), "status = 'error'");
  await user.click(screen.getByRole("radio", { name: "Spans" }));

  await waitFor(() =>
    expect(bodies.at(-1)).toEqual({ target: "span", filter: "status = 'error'" }),
  );
  expect(bodies.filter((body) => body.target === "span")).toEqual([
    { target: "span", filter: "status = 'error'" },
  ]);
});

it("marks the counter busy and dims it while a count is pending", async () => {
  capturePreviews();
  const { user } = await renderApp(NEW_RULE);
  const status = await screen.findByRole("status");
  await screen.findByText(FIXTURE_TEXT);
  expect(status).not.toHaveAttribute("aria-busy");

  await user.type(screen.getByLabelText("Filter"), "status = 'error'");

  expect(status).toHaveAttribute("aria-busy", "true");
  expect(status).toHaveClass("opacity-60");
  await waitFor(() => expect(status).not.toHaveAttribute("aria-busy"));
  expect(status).not.toHaveClass("opacity-60");
});

it("hides the sampled estimate when the count is capped", async () => {
  capturePreviews(() => HttpResponse.json({ ...previewFixture, matched: 10000, capped: true }));
  const { user } = await renderApp(NEW_RULE);
  await screen.findByText("More than 10,000 traces matched in the last 24 hours");

  await user.clear(screen.getByLabelText("Sample rate (%)"));
  await user.type(screen.getByLabelText("Sample rate (%)"), "25");

  expect(screen.getByRole("status")).toHaveTextContent(
    /^More than 10,000 traces matched in the last 24 hours$/,
  );
});
