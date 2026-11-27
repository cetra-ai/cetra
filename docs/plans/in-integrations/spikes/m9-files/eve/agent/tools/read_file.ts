import { defineTool } from "eve/tools";
import { z } from "zod";

export default defineTool({
  description: "Read a text file and return its contents.",
  inputSchema: z.object({ path: z.string().min(1) }),
  async execute({ path }) {
    return { path, text: "hello from the M9 spike" };
  },
});
