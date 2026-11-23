// SPDX-License-Identifier: Elastic-2.0
import { describe, expect, it } from "vitest";
import type { Schema } from "@/api/types";
import {
  attributesBody,
  certificateState,
  refreshErrorMessage,
  splitCertificates,
} from "@/ee/org-sso/saml";

const DAY_MS = 86_400_000;
const NOW = Date.parse("2026-06-01T12:00:00Z");

function certificate(notBefore: string, notAfter: string): Schema<"OrgSsoCertificate"> {
  return { sha256: "a".repeat(64), not_before: notBefore, not_after: notAfter };
}

describe("certificateState", () => {
  it("warns 30 days before expiry and not 31", () => {
    const at30 = certificate("2025-01-01T00:00:00Z", new Date(NOW + 30 * DAY_MS).toISOString());
    const at31 = certificate("2025-01-01T00:00:00Z", new Date(NOW + 31 * DAY_MS).toISOString());
    expect(certificateState(at30, NOW)).toEqual({ kind: "expiring", days: 30 });
    expect(certificateState(at31, NOW)).toEqual({ kind: "valid" });
  });

  it("reports an expired certificate", () => {
    const expired = certificate("2020-01-01T00:00:00Z", "2026-05-31T00:00:00Z");
    expect(certificateState(expired, NOW)).toEqual({ kind: "expired" });
  });

  it("reports the epoch dates of an unreadable certificate", () => {
    const epoch = certificate("1970-01-01T00:00:00Z", "1970-01-01T00:00:00Z");
    expect(certificateState(epoch, NOW)).toEqual({ kind: "unreadable" });
  });
});

describe("splitCertificates", () => {
  it("splits two PEM blocks", () => {
    const first = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----";
    const second = "-----BEGIN CERTIFICATE-----\nBBBB\n-----END CERTIFICATE-----";
    expect(splitCertificates(`${first}\n${second}\n`)).toEqual([first, second]);
  });

  it("drops the text before the first block", () => {
    const block = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----";
    expect(splitCertificates(`Signing key:\n${block}`)).toEqual([block]);
  });

  it("keeps text without a BEGIN marker as one entry", () => {
    expect(splitCertificates("  MIIBkey  ")).toEqual(["MIIBkey"]);
  });

  it("returns no entries for blank input", () => {
    expect(splitCertificates("   \n")).toEqual([]);
  });
});

describe("refreshErrorMessage", () => {
  it.each([
    ["fetch_failed", "The metadata URL could not be reached."],
    ["status", "The metadata URL answered with an error."],
    ["too_large", "The metadata is larger than 1 MiB."],
    ["denied", "The metadata URL is not allowed by this instance's network policy."],
    ["unusable", "The metadata could not be used."],
    [
      "entity_id_changed",
      "The metadata now names another entity ID. Save it again to accept the change.",
    ],
  ] as const)("explains %s", (error, message) => {
    expect(refreshErrorMessage(error)).toBe(message);
  });
});

describe("attributesBody", () => {
  it("drops blank names and trims the rest", () => {
    expect(
      attributesBody({ email: "  mail ", display_name: "  ", first_name: "", last_name: "sn" }),
    ).toEqual({ email: "mail", last_name: "sn" });
  });

  it("is empty when every name is blank", () => {
    expect(attributesBody({ email: " ", first_name: "" })).toEqual({});
  });
});
