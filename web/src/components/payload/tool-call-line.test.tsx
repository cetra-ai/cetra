import { fireEvent, render, screen } from "@testing-library/react";
import { expect, it, vi } from "vitest";
import { ToolCallLine } from "@/components/payload/tool-call-line";

it("shows the call on one line, with the result and the duration", () => {
  render(
    <ToolCallLine
      name="get_order_status"
      id="call_1"
      args={{ value: { order_id: "1234" } }}
      result={{ value: { status: "shipped" } }}
      durationNs={350_000_000}
    />,
  );
  const line = screen.getByRole("button");
  expect(line.textContent).toBe('get_order_status(order_id: "1234") → {status: "shipped"} · 350ms');
  expect(line).toHaveAttribute("aria-expanded", "false");
  expect(screen.queryByRole("region", { name: "Arguments" })).not.toBeInTheDocument();
  // The duration is outside the truncated text, so a long line never hides it.
  const duration = screen.getByText("· 350ms");
  expect(duration.closest(".truncate")).toBeNull();
});

it("points the open line at its body", () => {
  render(<ToolCallLine name="run" args={{ value: { a: 1 } }} />);
  const line = screen.getByRole("button");
  fireEvent.click(line);
  const body = document.getElementById(line.getAttribute("aria-controls") ?? "");
  expect(body).toContainElement(screen.getByRole("region", { name: "Arguments" }));
});

it("leaves out the sections of absent values", () => {
  render(<ToolCallLine name="ping" args={{ value: undefined }} result={{ value: undefined }} />);
  const line = screen.getByRole("button");
  expect(line.textContent).toBe("ping()");
  fireEvent.click(line);
  expect(screen.queryByRole("region", { name: "Arguments" })).not.toBeInTheDocument();
  expect(screen.queryByRole("region", { name: "Result" })).not.toBeInTheDocument();
});

it("opens on a click and closes on the next", () => {
  render(
    <ToolCallLine
      name="get_order_status"
      id="call_1"
      args={{ value: { order_id: "1234" } }}
      result={{ value: { status: "shipped" } }}
      durationNs={350_000_000}
    />,
  );
  const line = screen.getByRole("button");
  fireEvent.click(line);
  expect(line).toHaveAttribute("aria-expanded", "true");
  expect(screen.getByRole("region", { name: "Arguments" })).toBeInTheDocument();
  expect(screen.getByRole("region", { name: "Result" })).toBeInTheDocument();
  expect(screen.getByText("call_1")).toBeInTheDocument();

  fireEvent.click(line);
  expect(line).toHaveAttribute("aria-expanded", "false");
  expect(screen.queryByRole("region", { name: "Arguments" })).not.toBeInTheDocument();
});

it("shows an error in place of the result, and opens to the error text", () => {
  render(
    <ToolCallLine
      name="run_tests"
      id="call_2"
      args={{ value: { path: "tests" } }}
      result={{ value: { status: "shipped" } }}
      error="1 failed, 47 passed"
    />,
  );
  const line = screen.getByRole("button");
  expect(line.textContent).toContain("1 failed, 47 passed");
  expect(line.textContent).not.toContain("shipped");
  expect(screen.getByRole("img", { name: "Failed" })).toBeInTheDocument();
  expect(line).toHaveAttribute("data-status", "error");

  fireEvent.click(line);
  expect(screen.getByText("1 failed, 47 passed", { selector: "p" })).toBeInTheDocument();
  expect(screen.getByRole("region", { name: "Result" })).toBeInTheDocument();
});

it("calls onClick once per click", () => {
  const onClick = vi.fn();
  render(<ToolCallLine name="run" args={{ value: { a: 1 } }} onClick={onClick} />);
  const line = screen.getByRole("button");
  fireEvent.click(line);
  expect(onClick).toHaveBeenCalledTimes(1);
  fireEvent.click(line);
  expect(onClick).toHaveBeenCalledTimes(2);
});

it("reads a response without its call as name → result", () => {
  render(<ToolCallLine name="response" result={{ value: { ok: true } }} />);
  const line = screen.getByRole("button");
  expect(line.textContent).toBe("response → {ok: true}");
  fireEvent.click(line);
  expect(screen.queryByRole("region", { name: "Arguments" })).not.toBeInTheDocument();
});

it("reads a call with an empty result as name(args) only", () => {
  render(<ToolCallLine name="run" args={{ value: { a: 1 } }} result={{ value: "" }} />);
  expect(screen.getByRole("button").textContent).toBe("run(a: 1)");
});
