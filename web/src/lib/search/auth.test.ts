import { expect, it } from "vitest";
import { parseLoginSearch, parseSetupSearch } from "./auth";

it("keeps a safe redirect, a known error and an email", () => {
  expect(
    parseLoginSearch({ redirect: "/acme", error: "signup_closed", email: "ada@example.com" }),
  ).toEqual({ redirect: "/acme", error: "signup_closed", email: "ada@example.com" });
});

it("drops an unsafe redirect, an unknown error and an empty email", () => {
  const result = parseLoginSearch({ redirect: "//evil.example", error: "nope", email: "  " });
  expect(Object.keys(result).sort()).toEqual([
    "email",
    "error",
    "password",
    "redirect",
    "signed_out",
  ]);
  expect(result.redirect).toBeUndefined();
  expect(result.error).toBeUndefined();
  expect(result.email).toBeUndefined();
});

it("drops values that are not strings", () => {
  const result = parseLoginSearch({ redirect: 5, error: 1, email: true });
  expect(Object.keys(result).sort()).toEqual([
    "email",
    "error",
    "password",
    "redirect",
    "signed_out",
  ]);
  expect(result.redirect).toBeUndefined();
  expect(result.error).toBeUndefined();
  expect(result.email).toBeUndefined();
});

it("reads the password flag from 1 and only from 1", () => {
  expect(parseLoginSearch({ password: "1" }).password).toBe(true);
  expect(parseLoginSearch({ password: 1 }).password).toBe(true);
  // The router validates its own output again (`?password=1` parses to `1`, then `true`).
  expect(parseLoginSearch({ password: true }).password).toBe(true);
  expect(parseLoginSearch({ password: "true" }).password).toBeUndefined();
  expect(parseLoginSearch({}).password).toBeUndefined();
});

it("reads the signed-out flag from 1 and only from 1", () => {
  expect(parseLoginSearch({ signed_out: "1" }).signed_out).toBe(true);
  expect(parseLoginSearch({ signed_out: 1 }).signed_out).toBe(true);
  expect(parseLoginSearch({ signed_out: true }).signed_out).toBe(true);
  expect(parseLoginSearch({ signed_out: "true" }).signed_out).toBeUndefined();
  expect(parseLoginSearch({}).signed_out).toBeUndefined();
});

it("keeps a setup token and drops an empty one", () => {
  expect(parseSetupSearch({ token: "abc" })).toEqual({ token: "abc" });
  const empty = parseSetupSearch({ token: "" });
  expect("token" in empty).toBe(true);
  expect(empty.token).toBeUndefined();
  expect(parseSetupSearch({ token: "x".repeat(257) }).token).toBeUndefined();
});
