import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const root = new URL("../", import.meta.url);

test("Astro config targets the GitHub Pages project path", async () => {
  const config = await readFile(new URL("astro.config.mjs", root), "utf8");

  assert.match(config, /site:\s*["']https:\/\/cyjin-yl\.github\.io["']/);
  assert.match(config, /base:\s*["']\/z3rm["']/);
  assert.match(config, /output:\s*["']static["']/);
});

test("both public locales have an index route", async () => {
  for (const locale of ["en", "zh"]) {
    const page = await readFile(new URL(`src/pages/${locale}/index.astro`, root), "utf8");
    assert.match(page, /Z3rm/);
  }
});
