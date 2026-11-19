/**
 * A UUIDv7: a 48-bit Unix time in milliseconds, then random bits (RFC 9562), lowercase hex. `crypto.randomUUID` is not used because
 * it is missing over plain http (not a secure context); `getRandomValues` is not. Ids made
 * at later times sort later. `now` is for tests.
 */
export function uuidv7(now: number = Date.now()): string {
  const bytes = new Uint8Array(16);
  crypto.getRandomValues(bytes);
  let millis = now;
  for (let at = 5; at >= 0; at -= 1) {
    bytes[at] = millis % 256;
    millis = Math.floor(millis / 256);
  }
  bytes[6] = ((bytes[6] ?? 0) & 0x0f) | 0x70;
  bytes[8] = ((bytes[8] ?? 0) & 0x3f) | 0x80;
  const hex = Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}
