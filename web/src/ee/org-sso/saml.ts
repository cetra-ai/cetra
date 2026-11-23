// SPDX-License-Identifier: Elastic-2.0
import type { Schema } from "@/api/types";

/** The 409 detail the API sends when an entity ID change needs confirmation (M2 SA2.4). */
export const ENTITY_ID_CHANGE_DETAIL =
  "Changing the IdP entity ID deletes the linked identities; confirm the change.";

/** Days before `not_after` from which a certificate is shown as expiring (D-SA-11). */
export const EXPIRY_WARNING_DAYS = 30;

export type CertificateState =
  | { kind: "unreadable" }
  | { kind: "expired" }
  | { kind: "expiring"; days: number }
  | { kind: "valid" };

/** The display state of a certificate at `nowMs` (epoch dates mean it no longer reads). */
export function certificateState(
  certificate: Schema<"OrgSsoCertificate">,
  nowMs: number,
): CertificateState {
  const notBefore = Date.parse(certificate.not_before);
  const notAfter = Date.parse(certificate.not_after);
  if (notBefore <= 0 && notAfter <= 0) {
    return { kind: "unreadable" };
  }
  if (notAfter <= nowMs) {
    return { kind: "expired" };
  }
  const days = Math.ceil((notAfter - nowMs) / 86_400_000);
  if (days <= EXPIRY_WARNING_DAYS) {
    return { kind: "expiring", days };
  }
  return { kind: "valid" };
}

const CERTIFICATE_BEGIN = "-----BEGIN CERTIFICATE-----";

/** Splits pasted PEM text into certificates: one per BEGIN block, or the whole text when it has none. Blank input is []. */
export function splitCertificates(text: string): string[] {
  const trimmed = text.trim();
  if (trimmed === "") {
    return [];
  }
  if (!trimmed.includes(CERTIFICATE_BEGIN)) {
    return [trimmed];
  }
  return trimmed
    .split(/(?=-----BEGIN CERTIFICATE-----)/)
    .map((piece) => piece.trim())
    .filter((piece) => piece.startsWith(CERTIFICATE_BEGIN));
}

const REFRESH_ERROR_MESSAGES: Record<Schema<"SamlRefreshError">, string> = {
  fetch_failed: "The metadata URL could not be reached.",
  status: "The metadata URL answered with an error.",
  too_large: "The metadata is larger than 1 MiB.",
  denied: "The metadata URL is not allowed by this instance's network policy.",
  unusable: "The metadata could not be used.",
  entity_id_changed: "The metadata now names another entity ID. Save it again to accept the change.",
};

/** The message for a stored refresh failure (D-SA-11). */
export function refreshErrorMessage(error: Schema<"SamlRefreshError">): string {
  return REFRESH_ERROR_MESSAGES[error];
}

/** The attributes object of a PUT body: only the non-blank, trimmed names. */
export function attributesBody(
  names: Schema<"SamlAttributeNames">,
): Schema<"SamlAttributeNames"> {
  const body: Schema<"SamlAttributeNames"> = {};
  const keys = ["email", "display_name", "first_name", "last_name"] as const;
  for (const key of keys) {
    const value = names[key]?.trim();
    if (value) {
      body[key] = value;
    }
  }
  return body;
}
