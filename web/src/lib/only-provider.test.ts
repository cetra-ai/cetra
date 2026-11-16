import { expect, it } from "vitest";
import authConfig from "../../../fixtures/api/static/auth-config.json";
import { onlyProvider } from "./only-provider";

const CLOUD = { id: "oidc:cloud", display_name: "cetra Cloud" };
// The JSON import types `signup` as `string`; `AuthConfig` needs the union.
const BASE = { ...authConfig, signup: "invite_only" as const };

it("returns the provider when password sign-in is off and it is the only one", () => {
  expect(onlyProvider({ ...BASE, password_enabled: false, providers: [CLOUD] })).toEqual(CLOUD);
});

it("returns nothing while password sign-in is on", () => {
  expect(onlyProvider({ ...BASE, password_enabled: true, providers: [CLOUD] })).toBe(undefined);
});

it("returns nothing while org SSO discovery is on", () => {
  expect(
    onlyProvider({
      ...BASE,
      password_enabled: false,
      sso_discovery: true,
      providers: [CLOUD],
    }),
  ).toBe(undefined);
});

it("returns nothing with two providers", () => {
  expect(onlyProvider({ ...BASE, password_enabled: false })).toBe(undefined);
});

it("returns nothing without a provider", () => {
  expect(onlyProvider({ ...BASE, password_enabled: false, providers: [] })).toBe(undefined);
});
