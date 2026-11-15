import { render, screen } from "@testing-library/react";
import { expect, it } from "vitest";
import { OutputDiff } from "./output-diff";

it("marks the changed words", () => {
  const { container } = render(<OutputDiff before="the cat sat" after="the dog sat" />);

  expect(container.querySelector("ins")?.textContent).toBe("dog ");
  expect(container.querySelector("del")?.textContent).toBe("cat ");
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
