import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const root = new URL("../", import.meta.url);

async function source(path) { return readFile(new URL(path, root), "utf8"); }

test("site shell exposes primary navigation and preferences", async () => {
  const header = await source("src/components/SiteHeader.astro");
  for (const label of ["Features", "Guides", "Reference", "Status", "GitHub"]) assert.match(header, new RegExp(label));
  assert.match(header, /proto-ui-base-toggle/);
  assert.match(header, /z3rm-locale/);
});

test("documentation shell has navigable document landmarks", async () => {
  const layout = await source("src/layouts/DocsLayout.astro");
  assert.match(layout, /DocsSidebar/);
  assert.match(layout, /PageToc/);
  assert.match(layout, /<main/);
  assert.match(layout, /<aside/);
});

test("site layout registers Proto UI web components", async () => {
  const layout = await source("src/layouts/SiteLayout.astro");
  assert.match(layout, /proto-ui\/components\/wc\/index/);
  assert.match(layout, /<SiteHeader/);
  assert.match(layout, /<SiteFooter/);
});
