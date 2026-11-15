import { render, screen } from "@testing-library/react";
import { expect, it } from "vitest";
import { OutputDiff } from "./output-diff";

it("marks the changed words", () => {
  const { container } = render(<OutputDiff before="the cat sat" after="the dog sat" />);

  expect(container.querySelector("ins")?.textContent).toBe("added: dog ");
  expect(container.querySelector("del")?.textContent).toBe("removed: cat ");
});

it("shows equal outputs without marks", () => {
  const value = { answer: "Use the Forgot password link." };
  const { container } = render(<OutputDiff before={value} after={{ ...value }} />);

  expect(container.querySelector("ins")).toBeNull();
  expect(container.querySelector("del")).toBeNull();
  expect(screen.getByText("same as baseline")).toBeInTheDocument();
  expect(container).toHaveTextContent('"answer": "Use the Forgot password link."');
});

it("does not diff a very long output", () => {
  const after = Array.from({ length: 2001 }, (_, index) => `word${index}`).join(" ");
  const { container } = render(<OutputDiff before="short" after={after} />);

  expect(screen.getByText("Too long to diff")).toBeInTheDocument();
  expect(container.querySelector("ins")).toBeNull();
  expect(container.querySelector("del")).toBeNull();
});

it("shows a change after the first 2,000 characters", () => {
  const shared = "word ".repeat(500);
  const { container } = render(
    <OutputDiff before={`${shared}old tail`} after={`${shared}new tail`} />,
  );

  expect(container.querySelector("ins")?.textContent).toBe("added: new ");
  expect(container.querySelector("del")?.textContent).toBe("removed: old ");
});

it("cuts a long unchanged run to its context on each side of a change", () => {
  const head = "a".repeat(200);
  const tail = "b".repeat(200);
  const { container } = render(
    <OutputDiff before={`${head} old ${tail}`} after={`${head} new ${tail}`} />,
  );

  const text = container.querySelector("p")?.textContent ?? "";
  expect(text).toContain("…");
  expect(text).toContain("b".repeat(40));
  expect(text).not.toContain("a".repeat(41));
  expect(text).not.toContain("b".repeat(41));
});

it("shows the candidate with a note when the baseline output is absent", () => {
  const { container } = render(<OutputDiff before={undefined} after="a new answer" />);

  expect(screen.getByText("No baseline output")).toBeInTheDocument();
  expect(container).toHaveTextContent("a new answer");
  expect(container.querySelector("ins")).toBeNull();
});

it("shows no output when the candidate output is absent", () => {
  render(<OutputDiff before="the baseline" after={undefined} />);

  expect(screen.getByText("No output")).toBeInTheDocument();
});
