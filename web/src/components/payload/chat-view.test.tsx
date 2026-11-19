import { fireEvent, render, screen } from "@testing-library/react";
import { afterEach, expect, it, vi } from "vitest";
import { ChatView } from "@/components/payload/chat-view";

afterEach(() => {
  vi.restoreAllMocks();
});

it("collapses system instructions to one line with a line count", () => {
  render(
    <ChatView
      input={[]}
      output={[]}
      systemInstructions={"first line\nsecond line\nthird line"}
      projectRef="acme/bot"
    />,
  );
  expect(screen.getByText("3 lines")).toBeInTheDocument();
  expect(screen.getByText("first line")).toBeInTheDocument();
  expect(screen.queryByText(/second line/)).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "System instructions" }));
  expect(screen.getByText(/second line/)).toBeInTheDocument();
});

it("collapses reasoning until it is opened", () => {
  render(
    <ChatView
      input={[
        {
          role: "assistant",
          parts: [
            { type: "reasoning", content: "secret plan" },
            { type: "text", content: "hi" },
          ],
        },
      ]}
      output={[]}
      projectRef="acme/bot"
    />,
  );
  expect(screen.getByRole("button", { name: /Reasoning/ })).toHaveTextContent("11");
  expect(screen.queryByText("secret plan")).not.toBeInTheDocument();
  expect(screen.getByText("hi")).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: /Reasoning/ }));
  expect(screen.getByText("secret plan")).toBeInTheDocument();
});

it("highlights tools that this span called", () => {
  render(
    <ChatView
      input={[]}
      output={[
        {
          role: "assistant",
          parts: [{ type: "tool_call", id: "call_1", name: "lookup", arguments: { q: "1" } }],
        },
      ]}
      toolDefinitions={[
        { name: "lookup", description: "Find a record", parameters: { type: "object" } },
        { name: "unused", description: "Not called" },
      ]}
      projectRef="acme/bot"
    />,
  );
  fireEvent.click(screen.getByRole("button", { name: "Tools (2)" }));
  expect(document.querySelector("mark")).toHaveTextContent("lookup");
  expect(screen.getByText("unused").closest("mark")).toBeNull();
});

it("shows each message as a card with its role", () => {
  render(
    <ChatView
      input={[{ role: "user", parts: [{ type: "text", content: "Where is my order?" }] }]}
      output={[{ role: "assistant", parts: [{ type: "text", content: "On its way." }] }]}
      projectRef="acme/bot"
    />,
  );
  expect(screen.getByText("User")).toBeInTheDocument();
  expect(screen.getByText("Assistant")).toBeInTheDocument();
  expect(screen.getByText("Where is my order?")).toBeInTheDocument();
});

it("clips a long message until it is expanded", () => {
  vi.spyOn(HTMLElement.prototype, "scrollHeight", "get").mockReturnValue(2_000);
  vi.spyOn(HTMLElement.prototype, "clientWidth", "get").mockReturnValue(400);
  render(
    <ChatView
      input={[{ role: "user", parts: [{ type: "text", content: "long message" }] }]}
      output={[]}
      projectRef="acme/bot"
    />,
  );
  fireEvent.click(screen.getByRole("button", { name: "Show more" }));
  expect(screen.getByRole("button", { name: "Show less" })).toBeInTheDocument();
});
