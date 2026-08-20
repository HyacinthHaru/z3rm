import { defineCollection } from "astro:content";
import { glob } from "astro/loaders";
import { z } from "astro/zod";

const docs = defineCollection({
  loader: glob({ pattern: "**/*.md", base: "./src/content/docs" }),
  schema: z.object({
    title: z.string(),
    description: z.string(),
    translationKey: z.string(),
    section: z.enum(["home", "features", "guide", "concepts", "reference", "support", "status"]),
    order: z.number().int().nonnegative(),
    status: z.enum(["verified", "experimental", "mixed"]).default("verified"),
    navTitle: z.string().optional(),
  }),
});

export const collections = { docs };
