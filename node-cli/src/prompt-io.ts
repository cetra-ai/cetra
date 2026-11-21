import { createInterface } from "node:readline/promises";

/** True when both stdin and stderr are terminals, so questions can be asked. */
export function interactive(): boolean {
  return Boolean(process.stdin.isTTY && process.stderr.isTTY);
}

/** Asks one question on `output` and reads one answer from `input`. An empty answer gives
 *  `fallback`. The interface is closed in every case, or the process would not exit. */
export async function ask(
  question: string,
  fallback: string,
  input: NodeJS.ReadableStream = process.stdin,
  output: NodeJS.WritableStream = process.stderr,
): Promise<string> {
  const rl = createInterface({ input, output });
  try {
    const answer = (await rl.question(question)).trim();
    return answer === "" ? fallback : answer;
  } finally {
    rl.close();
  }
}

/** Reads a yes/no answer: an empty answer gives `defaultYes`, `y` and `yes` (any case) give true,
 *  anything else gives false. */
export function yes(answer: string, defaultYes: boolean): boolean {
  const normalized = answer.trim().toLowerCase();
  if (normalized === "") return defaultYes;
  return normalized === "y" || normalized === "yes";
}
