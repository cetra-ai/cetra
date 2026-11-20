import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";

import {
  type ChatEntry,
  type ChatMessage,
  chatNames,
  PromptCompileError,
  render,
  renderChat,
  variables,
} from "./template.js";

type FixtureError = {
  kind: string;
  name?: string;
  reason?: string;
  line?: number;
  column?: number;
};

type TextCase = {
  name: string;
  template: string;
  values: Record<string, string>;
  expected?: string;
  error?: FixtureError;
  variables: string[] | null;
};

type ChatCase = {
  name: string;
  template: ChatEntry[];
  values: Record<string, string>;
  placeholders: Record<string, ChatMessage[]>;
  expected?: ChatMessage[];
  error?: FixtureError;
  variables: string[];
  placeholder_names: string[];
};

const fixture: { text: TextCase[]; chat: ChatCase[] } = JSON.parse(
  readFileSync(new URL("../../../fixtures/prompts/render.json", import.meta.url), "utf8"),
);

/** The thrown error, or a failing assertion when nothing is thrown. */
function thrown(fn: () => unknown): PromptCompileError {
  try {
    fn();
  } catch (error) {
    if (error instanceof PromptCompileError) {
      return error;
    }
    throw error;
  }
  throw new Error("expected a PromptCompileError");
}

/** The fields a fixture error describes, as the error carries them. */
function projected(error: PromptCompileError): object {
  return {
    kind: error.kind,
    variable: error.variable,
    reason: error.reason,
    line: error.line,
    column: error.column,
  };
}

function expectedOf(error: FixtureError): object {
  return {
    kind: error.kind,
    variable: error.name,
    reason: error.reason,
    line: error.line,
    column: error.column,
  };
}

describe("shared text cases", () => {
  for (const testCase of fixture.text) {
    it(testCase.name, () => {
      if (testCase.variables !== null) {
        expect(variables(testCase.template)).toEqual(testCase.variables);
      } else {
        expect(projected(thrown(() => variables(testCase.template)))).toEqual(
          expectedOf(testCase.error as FixtureError),
        );
      }
      if (testCase.error !== undefined) {
        expect(projected(thrown(() => render(testCase.template, testCase.values)))).toEqual(
          expectedOf(testCase.error),
        );
      } else {
        expect(render(testCase.template, testCase.values)).toBe(testCase.expected);
      }
    });
  }
});

describe("shared chat cases", () => {
  for (const testCase of fixture.chat) {
    it(testCase.name, () => {
      expect(chatNames(testCase.template)).toEqual({
        variables: testCase.variables,
        placeholders: testCase.placeholder_names,
      });
      if (testCase.error !== undefined) {
        expect(
          projected(
            thrown(() => renderChat(testCase.template, testCase.values, testCase.placeholders)),
          ),
        ).toEqual(expectedOf(testCase.error));
      } else {
        expect(renderChat(testCase.template, testCase.values, testCase.placeholders)).toEqual(
          testCase.expected,
        );
      }
    });
  }
});

describe("errors", () => {
  it("hold no template text", () => {
    const error = thrown(() => render("{{ secret_payload.x }}", {}));
    expect(error.message).toBe("invalid template at line 1, column 1: not_a_name");
    expect(error.message).not.toContain("secret_payload");

    const missing = thrown(() => render("{{secret_payload}}", {}));
    expect(missing.message).toBe("missing variable secret_payload");
  });

  it("reject a chat entry that is not a message", () => {
    const notAMessage = { kind: "invalid", reason: "not_a_message" };
    expect(projected(thrown(() => chatNames([null as unknown as ChatEntry])))).toEqual(notAMessage);
    expect(projected(thrown(() => renderChat([{ role: "user" } as ChatEntry], {}, {})))).toEqual(
      notAMessage,
    );
  });

  it("read only the given values, not inherited properties", () => {
    expect(projected(thrown(() => render("{{constructor}}", {})))).toEqual({
      kind: "missing_variable",
      variable: "constructor",
      reason: undefined,
      line: undefined,
      column: undefined,
    });
  });
});
