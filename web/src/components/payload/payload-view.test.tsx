import { fireEvent, render, screen } from "@testing-library/react";
import { beforeEach, expect, it } from "vitest";
import { PAYLOAD_FORMAT_KEY } from "@/components/payload/format-switch";
import { PayloadView } from "@/components/payload/payload-view";

beforeEach(() => {
  localStorage.removeItem(PAYLOAD_FORMAT_KEY);
});

it("remembers the format switch", () => {
  const payload = { format: "json" as const, value: { order_id: "1234" } };
  const first = render(<PayloadView label="Input" payload={payload} />);
  expect(screen.getByText("order_id")).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Raw" }));
  expect(screen.getByText('{"order_id":"1234"}')).toBeInTheDocument();
  first.unmount();
  render(<PayloadView label="Input" payload={payload} />);
  expect(screen.getByText('{"order_id":"1234"}')).toBeInTheDocument();
  expect(screen.queryByText("order_id")).not.toBeInTheDocument();
});

it("keeps two payload views on the same format", () => {
  render(
    <>
      <PayloadView label="Input" payload={{ format: "json", value: { a: 1 } }} />
      <PayloadView label="Output" payload={{ format: "json", value: { b: 2 } }} />
    </>,
  );
  const raw = screen.getAllByRole("button", { name: "Raw" });
  const first = raw[0];
  if (!first) {
    throw new Error("missing raw switch");
  }
  fireEvent.click(first);
  expect(screen.getByText('{"a":1}')).toBeInTheDocument();
  expect(screen.getByText('{"b":2}')).toBeInTheDocument();
});

it("shows a notice above 1 MB instead of rendering pretty", () => {
  const value = "x".repeat(1_000_001);
  render(<PayloadView label="Input" payload={{ format: "text", value }} />);
  expect(screen.getByText("Large payload (1.0 MB)")).toBeInTheDocument();
  expect(screen.queryByText(value)).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Render anyway" }));
  expect(screen.getByText(value)).toBeInTheDocument();
});

it("lists message roles and text in the pretty view", () => {
  render(
    <PayloadView
      label="Input"
      payload={{
        format: "messages",
        value: [
          {
            role: "user",
            parts: [{ type: "text", content: "Where is order 1234?" }],
          },
        ],
      }}
    />,
  );
  expect(screen.getByText("User")).toBeInTheDocument();
  expect(screen.getByText("Where is order 1234?")).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "JSON" }));
  expect(screen.getByRole("button", { name: "Expand parts" })).toBeInTheDocument();
  expect(screen.queryByText("Where is order 1234?")).not.toBeInTheDocument();
});

it("renders text as Markdown in pretty and as plain text in raw", () => {
  const text = "## Steps\n\n- first **item**";
  render(<PayloadView label="Output" payload={{ format: "text", value: text }} />);
  expect(screen.getByRole("heading", { level: 2, name: "Steps" })).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Raw" }));
  expect(screen.queryByRole("heading", { level: 2 })).not.toBeInTheDocument();
  expect(document.querySelector("pre")).toHaveTextContent("- first **item**");
});

it("shows text as plain text in JSON mode too", () => {
  render(<PayloadView label="Output" payload={{ format: "text", value: "## Not a heading" }} />);
  fireEvent.click(screen.getByRole("button", { name: "JSON" }));
  expect(screen.queryByRole("heading", { level: 2 })).not.toBeInTheDocument();
  expect(screen.getByText("## Not a heading")).toBeInTheDocument();
});

it("renders an llm span's messages as cards with a Markdown answer", () => {
  render(
    <PayloadView
      label="Output"
      payload={{
        format: "messages",
        value: [
          {
            role: "assistant",
            parts: [{ type: "text", content: "Here is code:\n\n```ts\nconst x = 1;\n```" }],
          },
        ],
      }}
    />,
  );
  expect(screen.getByText("Assistant")).toBeInTheDocument();
  expect(screen.getByText("ts")).toBeInTheDocument();
  expect(document.querySelector("pre code.language-ts")).not.toBeNull();
});

it("frames text and JSON payloads in a card, and leaves message cards unframed", () => {
  const view = render(
    <>
      <PayloadView label="Input" payload={{ format: "text", value: "How do I reset it?" }} />
      <PayloadView
        label="Output"
        payload={{
          format: "messages",
          value: [{ role: "assistant", parts: [{ type: "text", content: "Use the link." }] }],
        }}
      />
    </>,
  );
  const text = screen.getByText("How do I reset it?");
  expect(text.closest(".rounded-md.border.bg-card")).not.toBeNull();
  const reply = screen.getByText("Use the link.");
  expect(
    reply.closest("article")?.parentElement?.closest(".border-l-muted-foreground\\/40"),
  ).toBeNull();
  view.unmount();
});
