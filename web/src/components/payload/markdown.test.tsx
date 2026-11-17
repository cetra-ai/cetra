import { render, screen } from "@testing-library/react";
import { expect, it } from "vitest";
import { Markdown } from "@/components/payload/markdown";

it("renders a heading, a list, a link and inline code", () => {
  render(
    <Markdown
      text={
        "## Refund policy\n\n- Refunds within `30` days\n- Contact support\n\nSee [the docs](https://example.com/docs)."
      }
    />,
  );
  expect(screen.getByRole("heading", { level: 2, name: "Refund policy" })).toBeInTheDocument();
  expect(screen.getByRole("list")).toHaveTextContent("Refunds within 30 days");
  const link = screen.getByRole("link", { name: "the docs" });
  expect(link).toHaveAttribute("href", "https://example.com/docs");
  expect(link).toHaveAttribute("target", "_blank");
  expect(link).toHaveAttribute("rel", "noopener noreferrer");
  expect(screen.getByText("30").tagName).toBe("CODE");
});

it("renders a fenced block with its language and a copy button", () => {
  render(<Markdown text={"```python\nprint('hi')\n```"} />);
  expect(screen.getByText("python")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Copy code" })).toBeInTheDocument();
  expect(document.querySelector("pre code.language-python")).not.toBeNull();
});

it("labels a fenced block without a language as text and does not guess one", () => {
  render(<Markdown text={"```\nplain words\n```"} />);
  expect(screen.getByText("text")).toBeInTheDocument();
  expect(document.querySelector("pre code.hljs")).toBeNull();
  expect(document.querySelector("pre span")).toBeNull();
});

it("shows raw HTML as text instead of parsing it", () => {
  const { container } = render(<Markdown text={'<span data-x="1">raw html</span>'} />);
  expect(container.querySelector("span[data-x]")).toBeNull();
  expect(screen.getByText('<span data-x="1">raw html</span>')).toBeInTheDocument();
});
