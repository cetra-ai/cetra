import { expect, it } from "vitest";
import { decodeValue, userIdentity } from "@/lib/user-identity";

it("reads the name, email and plan from the user keys, decoded", () => {
  expect(
    userIdentity({
      name: '"Ada Lovelace"',
      email: '"ada@example.test"',
      plan: '"team"',
      "other.key": '"ignored"',
    }),
  ).toEqual({ name: "Ada Lovelace", email: "ada@example.test", plan: "team" });
});

it("has no identity without metadata", () => {
  expect(userIdentity(undefined)).toEqual({});
  expect(userIdentity({})).toEqual({});
});

it("treats an empty name as absent and keeps a non-JSON value as it is", () => {
  expect(userIdentity({ name: '""', plan: "pro" })).toEqual({ plan: "pro" });
  expect(userIdentity({ name: "  Ada  " })).toEqual({ name: "Ada" });
});

it("decodes JSON strings and numbers and keeps other text", () => {
  expect(decodeValue('"x"')).toBe("x");
  expect(decodeValue("42")).toBe("42");
  expect(decodeValue("not json")).toBe("not json");
});
