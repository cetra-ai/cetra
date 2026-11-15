import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { describe, expect, it } from "vitest";
import { EvaluateSnippet } from "./evaluate-snippet";

describe("EvaluateSnippet", () => {
  it("quotes the dataset name in the Python code", () => {
    const { container } = render(<EvaluateSnippet datasetName='faq "v2"' />);
    const code = container.querySelector("pre code")?.textContent ?? "";
    expect(code).toContain('dataset="faq \\"v2\\""');
  });

  it("shows the TypeScript code in its tab", async () => {
    const user = userEvent.setup();
    render(<EvaluateSnippet datasetName="faq" />);
    await user.click(screen.getByRole("tab", { name: "TypeScript" }));
    expect(await screen.findByText(/llmobs\.evaluate\(\{/)).toBeInTheDocument();
  });
});
