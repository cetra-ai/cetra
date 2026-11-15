import { afterEach, expect, it, vi } from "vitest";
import { submitExternalForm } from "@/lib/external-navigation";

afterEach(() => {
  vi.restoreAllMocks();
});

it("posts the fields as hidden inputs, submits once, and removes the form", () => {
  const seen: { method: string; action: string; inputs: unknown[]; attached: boolean }[] = [];
  const submit = vi.spyOn(HTMLFormElement.prototype, "submit").mockImplementation(function (
    this: HTMLFormElement,
  ) {
    seen.push({
      method: this.method,
      action: this.action,
      attached: document.body.contains(this),
      inputs: Array.from(this.querySelectorAll("input"), (input) => ({
        name: input.name,
        type: input.type,
        value: input.value,
      })),
    });
  });

  submitExternalForm("http://127.0.0.1:54321/callback", {
    token: "pat-llmobs-A+B",
    state: "xyz",
  });

  expect(submit).toHaveBeenCalledTimes(1);
  expect(seen).toEqual([
    {
      method: "post",
      action: "http://127.0.0.1:54321/callback",
      attached: true,
      inputs: [
        { name: "token", type: "hidden", value: "pat-llmobs-A+B" },
        { name: "state", type: "hidden", value: "xyz" },
      ],
    },
  ]);
  expect(document.querySelector("form")).toBeNull();
});
