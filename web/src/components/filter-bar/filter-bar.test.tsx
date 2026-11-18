import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import { HttpResponse, http } from "msw";
import { expect, it } from "vitest";
import { server } from "@/mocks/node";
import { renderApp } from "@/test/render";

const FILTER = "status = 'error' AND model LIKE 'gpt%'";

function chip(label: string) {
  return (name: string) => name === label;
}

it("renders filter chips and removes one", async () => {
  const urls: URL[] = [];
  const onStart = ({ request }: { request: Request }) => {
    const url = new URL(request.url);
    if (url.pathname.endsWith("/traces")) {
      urls.push(url);
    }
  };
  server.events.on("request:start", onStart);
  try {
    const { router, user } = await renderApp(
      `/acme/support-bot/traces?filter=${encodeURIComponent(FILTER)}`,
    );
    expect(
      await screen.findByRole("button", { name: chip("status = 'error'") }),
    ).toBeInTheDocument();
    expect(screen.getByRole("button", { name: chip("model LIKE 'gpt%'") })).toBeInTheDocument();
    expect(urls.some((url) => url.searchParams.get("filter") === FILTER)).toBe(true);
    await user.click(screen.getByRole("button", { name: "Remove model LIKE 'gpt%'" }));
    await waitFor(() => {
      expect(router.state.location.search.filter).toBe("status = 'error'");
    });
  } finally {
    server.events.removeListener("request:start", onStart);
  }
});

it("edits a compare chip from the popover", async () => {
  const { router, user } = await renderApp(
    `/acme/support-bot/traces?filter=${encodeURIComponent(FILTER)}`,
  );
  await user.click(await screen.findByRole("button", { name: chip("status = 'error'") }));
  const value = await screen.findByRole("textbox", { name: "Value" });
  await user.clear(value);
  await user.type(value, "ok");
  await user.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Apply" }));
  await waitFor(() => {
    expect(router.state.location.search.filter).toBe("status = 'ok' AND model LIKE 'gpt%'");
  });
});

it("applies the text editor with Enter", async () => {
  const { router, user } = await renderApp("/acme/support-bot/traces?range=15m");
  await user.click(await screen.findByRole("button", { name: "Edit as text" }));
  const content = document.querySelector(".cm-content");
  if (!(content instanceof HTMLElement)) {
    throw new Error("filter editor did not mount");
  }
  content.focus();
  await user.type(content, "status = 'ok'{Enter}");
  await waitFor(() => {
    expect(router.state.location.search.filter).toBe("status = 'ok'");
  });
});

it("shows an invalid filter on the bar and no rows", async () => {
  server.use(
    http.get("*/api/v1/projects/:project/traces", () =>
      HttpResponse.json(
        {
          type: "about:blank",
          title: "Bad Request",
          status: 400,
          code: "invalid_filter",
          errors: [
            {
              field: "filter",
              message: "unknown column nme",
              position: { line: 1, column: 1 },
            },
          ],
        },
        { status: 400, headers: { "Content-Type": "application/problem+json" } },
      ),
    ),
  );
  await renderApp(`/acme/support-bot/traces?filter=${encodeURIComponent("nme = 1")}`);
  expect(await screen.findAllByText("unknown column nme")).not.toHaveLength(0);
  await waitFor(() => {
    expect(document.querySelector(".cm-filter-error")?.textContent).toBe("nme");
  });
  expect(screen.queryByRole("grid", { name: "Traces" })).not.toBeInTheDocument();
});

it("appends a status comparison from the row", async () => {
  const { router, user } = await renderApp("/acme/support-bot/traces?range=15m");
  await screen.findByRole("grid", { name: "Traces" });
  await user.click(screen.getByRole("button", { name: "Filter by status error" }));
  await waitFor(() => {
    expect(router.state.location.search.filter).toBe("status = 'error'");
  });
  expect(router.state.location.search.trace).toBeUndefined();
});

it("shows a filter with a top-level OR as one chip and removes it whole", async () => {
  const filter = "a = 1 OR b = 2 AND c = 3";
  const { router, user } = await renderApp(
    `/acme/support-bot/traces?range=1h&filter=${encodeURIComponent(filter)}`,
  );
  const remove = await screen.findByRole("button", { name: `Remove ${filter}` });
  expect(screen.queryByRole("button", { name: "Remove c = 3" })).not.toBeInTheDocument();
  await user.click(remove);
  await waitFor(() => {
    expect(router.state.location.search.filter).toBeUndefined();
  });
  expect(router.state.location.search.range).toBe("1h");
});

it("parenthesizes an OR filter before appending from a row", async () => {
  const { router, user } = await renderApp(
    `/acme/support-bot/traces?range=1h&filter=${encodeURIComponent("a = 1 OR b = 2")}`,
  );
  await screen.findByRole("grid", { name: "Traces" });
  await user.click(screen.getByRole("button", { name: "Filter by status error" }));
  await waitFor(() => {
    expect(router.state.location.search.filter).toBe("(a = 1 OR b = 2) AND status = 'error'");
  });
});

/** `/facets` answers two values of `status`, filtered by `q` like the server does (D7). */
function useStatusValues() {
  server.use(
    http.get("*/api/v1/projects/:project/facets", ({ request }) => {
      const url = new URL(request.url);
      const q = url.searchParams.get("q");
      const values = [
        { value: "error", count: 4 },
        { value: "ok", count: 9 },
      ].filter((item) => q === null || item.value.includes(q));
      return HttpResponse.json({
        view: "traces",
        field: url.searchParams.get("field"),
        kind: "values",
        total: 13,
        missing: 0,
        values,
        more: false,
      });
    }),
  );
}

async function pickStatusField(user: Awaited<ReturnType<typeof renderApp>>["user"]) {
  await user.click(await screen.findByRole("button", { name: "+ Filter" }));
  await user.click(await screen.findByRole("button", { name: "Field" }));
  await user.type(screen.getByPlaceholderText("Search fields…"), "status");
  await user.click(await screen.findByRole("option", { name: "status" }));
}

it("adds a chip from the + Filter form", async () => {
  const { router, user } = await renderApp("/acme/support-bot/traces?range=1h");
  await screen.findByRole("grid", { name: "Traces" });
  await pickStatusField(user);
  await user.type(screen.getByRole("textbox", { name: "Value" }), "error");
  await user.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Apply" }));
  await waitFor(() => {
    expect(router.state.location.search.filter).toBe("status = 'error'");
  });
});

it("keeps the existing filter text when a chip is added", async () => {
  const { router, user } = await renderApp(
    `/acme/support-bot/traces?range=1h&filter=${encodeURIComponent("model LIKE 'gpt%'")}`,
  );
  await screen.findByRole("button", { name: chip("model LIKE 'gpt%'") });
  await pickStatusField(user);
  await user.type(screen.getByRole("textbox", { name: "Value" }), "ok");
  await user.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Apply" }));
  await waitFor(() => {
    expect(router.state.location.search.filter).toBe("model LIKE 'gpt%' AND status = 'ok'");
  });
});

it("builds an IN list from tokens: Enter adds, Backspace removes the last", async () => {
  useStatusValues();
  const { router, user } = await renderApp("/acme/support-bot/traces?range=1h");
  await screen.findByRole("grid", { name: "Traces" });
  await pickStatusField(user);
  await user.selectOptions(screen.getByRole("combobox", { name: "Operator" }), "IN");
  const value = screen.getByRole("textbox", { name: "Value" });
  await user.type(value, "ok{Enter}error{Enter}");
  expect(screen.getByRole("button", { name: "Remove error" })).toBeInTheDocument();
  await user.keyboard("{Backspace}");
  expect(screen.queryByRole("button", { name: "Remove error" })).not.toBeInTheDocument();
  await user.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Apply" }));
  await waitFor(() => {
    expect(router.state.location.search.filter).toBe("status IN ('ok')");
  });
});

it("adds a typed token that was not entered when Apply is pressed", async () => {
  const { router, user } = await renderApp("/acme/support-bot/traces?range=1h");
  await screen.findByRole("grid", { name: "Traces" });
  await pickStatusField(user);
  await user.selectOptions(screen.getByRole("combobox", { name: "Operator" }), "IN");
  await user.type(screen.getByRole("textbox", { name: "Value" }), "ok{Enter}error");
  await user.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Apply" }));
  await waitFor(() => {
    expect(router.state.location.search.filter).toBe("status IN ('ok', 'error')");
  });
});

it("enables Apply for a typed token alone", async () => {
  const { router, user } = await renderApp("/acme/support-bot/traces?range=1h");
  await screen.findByRole("grid", { name: "Traces" });
  await pickStatusField(user);
  await user.selectOptions(screen.getByRole("combobox", { name: "Operator" }), "NOT IN");
  const apply = within(screen.getByRole("dialog")).getByRole("button", { name: "Apply" });
  expect(apply).toBeDisabled();
  await user.type(screen.getByRole("textbox", { name: "Value" }), "ok");
  expect(apply).toBeEnabled();
  await user.click(apply);
  await waitFor(() => {
    expect(router.state.location.search.filter).toBe("status NOT IN ('ok')");
  });
});

it("suggests the field's values from /facets and adds one on click", async () => {
  useStatusValues();
  const { router, user } = await renderApp("/acme/support-bot/traces?range=1h");
  await screen.findByRole("grid", { name: "Traces" });
  await pickStatusField(user);
  await user.type(screen.getByRole("textbox", { name: "Value" }), "e");
  await waitFor(() => {
    expect(screen.queryByRole("button", { name: /^ok/ })).not.toBeInTheDocument();
  });
  await user.click(await screen.findByRole("button", { name: /^error/ }));
  await user.click(within(screen.getByRole("dialog")).getByRole("button", { name: "Apply" }));
  await waitFor(() => {
    expect(router.state.location.search.filter).toBe("status = 'error'");
  });
});

it("completes a value in the text editor from /facets", async () => {
  useStatusValues();
  const { router, user } = await renderApp("/acme/support-bot/traces?range=1h");
  await user.click(await screen.findByRole("button", { name: "Edit as text" }));
  const content = document.querySelector(".cm-content");
  if (!(content instanceof HTMLElement)) {
    throw new Error("filter editor did not mount");
  }
  content.focus();
  await user.type(content, "status = e");
  await user.click(await screen.findByRole("option", { name: /'error'/ }));
  await waitFor(() => {
    expect(content.textContent).toBe("status = 'error'");
  });
  await user.keyboard("{Enter}");
  await waitFor(() => {
    expect(router.state.location.search.filter).toBe("status = 'error'");
  });
});

it("submits the chip on Enter when no token is being typed", async () => {
  useStatusValues();
  const { router, user } = await renderApp("/acme/support-bot/traces?range=1h");
  await screen.findByRole("grid", { name: "Traces" });
  await pickStatusField(user);
  await user.selectOptions(screen.getByRole("combobox", { name: "Operator" }), "IN");
  await user.type(screen.getByRole("textbox", { name: "Value" }), "ok{Enter}");
  await user.keyboard("{Enter}");
  await waitFor(() => {
    expect(router.state.location.search.filter).toBe("status IN ('ok')");
  });
});

it("ignores Enter while an input method is composing", async () => {
  const { user } = await renderApp("/acme/support-bot/traces?range=1h");
  await screen.findByRole("grid", { name: "Traces" });
  await pickStatusField(user);
  await user.selectOptions(screen.getByRole("combobox", { name: "Operator" }), "IN");
  const value = screen.getByRole("textbox", { name: "Value" });
  await user.type(value, "ok");
  fireEvent.keyDown(value, { key: "Enter", isComposing: true });
  expect(screen.queryByRole("button", { name: "Remove ok" })).not.toBeInTheDocument();
});

it("never shows the values of the previous field after the field changes", async () => {
  server.use(
    http.get("*/api/v1/projects/:project/facets", ({ request }) => {
      const url = new URL(request.url);
      const field = url.searchParams.get("field") ?? "";
      const values =
        field === "status" ? [{ value: "error", count: 4 }] : [{ value: "checkout", count: 2 }];
      return HttpResponse.json({
        view: "traces",
        field,
        kind: "values",
        total: 6,
        missing: 0,
        values,
        more: false,
      });
    }),
  );
  const { user } = await renderApp("/acme/support-bot/traces?range=1h");
  await screen.findByRole("grid", { name: "Traces" });
  await pickStatusField(user);
  await screen.findByRole("button", { name: /^error/ });
  await user.click(screen.getByRole("button", { name: "Field" }));
  await user.type(screen.getByPlaceholderText("Search fields…"), "name");
  await user.click(await screen.findByRole("option", { name: "name" }));
  expect(screen.queryByRole("button", { name: /^error/ })).not.toBeInTheDocument();
  await screen.findByRole("button", { name: /^checkout/ });
});

it("asks /facets once for a map key typed letter by letter", async () => {
  const urls: string[] = [];
  const onStart = ({ request }: { request: Request }) => {
    const url = new URL(request.url);
    if (url.searchParams.get("field")?.startsWith("metadata")) {
      urls.push(url.searchParams.get("field") ?? "");
    }
  };
  server.events.on("request:start", onStart);
  try {
    const { user } = await renderApp("/acme/support-bot/traces?range=1h");
    await screen.findByRole("grid", { name: "Traces" });
    await user.click(await screen.findByRole("button", { name: "+ Filter" }));
    await user.click(await screen.findByRole("button", { name: "Field" }));
    await user.type(screen.getByPlaceholderText("Search fields…"), "metadata");
    await user.click(await screen.findByRole("option", { name: "metadata" }));
    await user.type(screen.getByRole("textbox", { name: "Key" }), "env");
    await new Promise((resolve) => window.setTimeout(resolve, 300));
    expect(urls.filter((field) => field === "metadata['env']")).toHaveLength(1);
  } finally {
    server.events.removeListener("request:start", onStart);
  }
});
