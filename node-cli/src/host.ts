import { hostname } from "node:os";

/** The machine's name for the approval page: `[A-Za-z0-9.-]` only, at most 64 characters.
 *  Undefined when nothing is left. The server sanitises it again. */
export function localHost(): string | undefined {
  const cleaned = hostname()
    .replace(/[^A-Za-z0-9.-]/g, "")
    .slice(0, 64);
  return cleaned === "" ? undefined : cleaned;
}
