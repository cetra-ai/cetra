import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { defineTool } from "eve/tools";
import { z } from "zod";

export default defineTool({
  description: "Read a text file and return its contents.",
  inputSchema: z.object({ path: z.string().min(1) }),
  async execute({ path }) {
    // The dev server runs from the scenario folder (its project root), while
    // hello.txt lives in the run's work directory, so the path is resolved
    // against the directory the scenario program points eve at.
    const work = process.env.CAPTURE_EVE_WORK;
    if (work === undefined) {
      throw new Error("CAPTURE_EVE_WORK is unset");
    }
    return { path, text: readFileSync(resolve(work, path), "utf8") };
  },
});
