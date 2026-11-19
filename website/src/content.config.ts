import { defineCollection, z } from "astro:content";
import { docsLoader } from "@astrojs/starlight/loaders";
import { docsSchema } from "@astrojs/starlight/schema";

// Until the first release every page says so (D-W-9). The release that
// publishes 0.1.0 deletes this default (website/README.md § Releases).
const NOT_RELEASED = {
  content: "cetra is not released yet. These docs describe the development version.",
};

export const collections = {
  docs: defineCollection({
    loader: docsLoader(),
    schema: docsSchema({
      extend: z.object({
        banner: z.object({ content: z.string() }).default(NOT_RELEASED),
      }),
    }),
  }),
};
