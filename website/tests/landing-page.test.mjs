import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const root = new URL("../", import.meta.url);

test("landing page presents the product through verified evidence", async () => {
  const source = await readFile(new URL("src/components/HomeLanding.astro", root), "utf8");
  for (const token of ["Z3RM-MUX-001", "Z3RM-SNAPSHOT-001", "z3rm-terminal-grid.png", "server-canonical-architecture.svg", "session-lifecycle.svg"]) {
    assert.match(source, new RegExp(token));
  }
  assert.match(source, /lang === "zh"/);
  assert.match(source, /quick-start/);
});

test("home route renders the dedicated landing experience", async () => {
  const source = await readFile(new URL("src/pages/[lang]/[...slug].astro", root), "utf8");
  assert.match(source, /HomeLanding/);
  assert.match(source, /<HomeLanding/);
});
