import { CliError, Exit } from "./errors.js";

const DURATION_ERROR = "--since expects a duration such as 30m, 24h or 7d";
const UNIT_SECONDS: Record<string, number> = { s: 1, m: 60, h: 3_600, d: 86_400 };

/** An instant as RFC 3339 with nine fractional digits and `Z`, the form the Rust CLI sends. */
function rfc3339Nanos(time: Date): string {
  return time.toISOString().replace(/\.(\d{3})Z$/, ".$1000000Z");
}

/** `[now - duration, now)` for `--since`, as the Rust `since_window`: the last character is the
 *  unit (`s`, `m`, `h` or `d`), the rest is 1 to 9 digits with no leading `0` and no sign. A window
 *  that starts before 1970-01-01 is refused. */
export function sinceWindow(text: string, now: Date): { from: string; to: string } {
  const unit = text.slice(-1);
  const digits = text.slice(0, -1);
  const multiplier = Object.hasOwn(UNIT_SECONDS, unit) ? UNIT_SECONDS[unit] : undefined;
  if (multiplier === undefined || !/^[1-9][0-9]{0,8}$/.test(digits)) {
    throw new CliError(DURATION_ERROR, Exit.Usage);
  }
  const from = new Date(now.getTime() - Number(digits) * multiplier * 1000);
  if (from.getTime() < 0) throw new CliError(DURATION_ERROR, Exit.Usage);
  return { from: rfc3339Nanos(from), to: rfc3339Nanos(now) };
}
