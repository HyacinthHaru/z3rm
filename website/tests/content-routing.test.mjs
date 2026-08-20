import assert from "node:assert/strict";
import { readFile, readdir } from "node:fs/promises";
import test from "node:test";

const root = new URL("../", import.meta.url);

async function markdownRoutes(locale) {
  const localeRoot = new URL(`src/content/docs/${locale}/`, root);
  const walk = async (url, prefix = "") => {
    const entries = await readdir(url, { withFileTypes: true });
    const routes = [];
    for (const entry of entries) {
      if (entry.isDirectory()) routes.push(...await walk(new URL(`${entry.name}/`, url), `${prefix}${entry.name}/`));
      else if (entry.name.endsWith(".md")) routes.push(`${prefix}${entry.name.slice(0, -3)}`);
    }
    return routes;
  };
  return (await walk(localeRoot)).sort();
}

test("English and Chinese content routes stay paired", async () => {
  assert.deepEqual(await markdownRoutes("en"), await markdownRoutes("zh"));
});

test("content schema requires translation and navigation metadata", async () => {
  const source = await readFile(new URL("src/content.config.ts", root), "utf8");
  for (const field of ["translationKey", "section", "order", "status"]) assert.match(source, new RegExp(field));
});

test("one dynamic route renders all localized documents", async () => {
  const source = await readFile(new URL("src/pages/[lang]/[...slug].astro", root), "utf8");
  assert.match(source, /getStaticPaths/);
  assert.match(source, /render\(/);
  assert.match(source, /hreflang/);
});
